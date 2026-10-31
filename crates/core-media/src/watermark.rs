//! 文字水印（C4 do 型变换：文字 + 位置 + 透明度）。
//!
//! 实现裁决：
//! - 字体内嵌（Noto Sans SC variable TTF，SIL OFL 1.1，见 assets/fonts/OFL.txt）——
//!   零运行时解析、零系统依赖，桌面/容器双端确定渲染；中英文同一套代码路径。
//! - 光栅化 `ab_glyph`（纯 Rust）；像素解码复用平台后端 `rgb8()`（白底展平语义与
//!   缩略图一致），混合与编码走 `encode_rgb_to`（双平台同语义）。
//! - 单行文本 v1（换行显式报错）；字形缺失（emoji 等）显式报错，不静默跳字。

use std::path::Path;

use ab_glyph::{Font as _, FontArc, Glyph, Point, PxScale, PxScaleFont, ScaleFont as _};

use crate::{encode_rgb_to, MediaError, Result};

/// 内嵌字体：Noto Sans SC variable（默认实例 wght=400），OFL 1.1 允许再分发/内嵌。
/// sha256 a3041811a78c361b1de50f953c805e0244951c21c5bd412f7232ef0d899af0da。
const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-wght.ttf");

/// 水印落位（相对画布）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkPosition {
    TopLeft,
    TopRight,
    Center,
    BottomLeft,
    BottomRight,
}

impl WatermarkPosition {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "top_left" => Ok(Self::TopLeft),
            "top_right" => Ok(Self::TopRight),
            "center" => Ok(Self::Center),
            "bottom_left" => Ok(Self::BottomLeft),
            "bottom_right" => Ok(Self::BottomRight),
            other => Err(MediaError::Img(format!(
                "未知水印位置 '{other}'（已知: top_left/top_right/center/bottom_left/bottom_right）"
            ))),
        }
    }
}

/// 文字水印参数（全部显式，插件层解析 YAML 后构造）。
#[derive(Debug, Clone, PartialEq)]
pub struct TextWatermark {
    pub text: String,
    pub position: WatermarkPosition,
    /// 0..=100（100 = 完全不透明）
    pub opacity: u8,
    /// 字号（像素，PxScale）
    pub font_size: f32,
    /// 距最近边距（像素）
    pub margin: i32,
    /// RGB 颜色
    pub color: [u8; 3],
}

impl TextWatermark {
    /// "RRGGBB" → [u8;3]。
    pub fn parse_color(hex: &str) -> Result<[u8; 3]> {
        let h = hex.trim();
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MediaError::Img(format!(
                "颜色须为 RRGGBB 六位十六进制，实际 '{hex}'"
            )));
        }
        Ok([
            u8::from_str_radix(&h[0..2], 16).map_err(|e| MediaError::Img(e.to_string()))?,
            u8::from_str_radix(&h[2..4], 16).map_err(|e| MediaError::Img(e.to_string()))?,
            u8::from_str_radix(&h[4..6], 16).map_err(|e| MediaError::Img(e.to_string()))?,
        ])
    }

    fn validate(&self, w: i32, h: i32) -> Result<()> {
        if self.text.is_empty() {
            return Err(MediaError::Img("水印文字为空".into()));
        }
        if self.text.contains('\n') {
            return Err(MediaError::Img("水印 v1 仅支持单行文本（含换行）".into()));
        }
        if self.opacity == 0 {
            return Err(MediaError::Img("opacity=0 无意义（拒绝静默空产物）".into()));
        }
        if !(8.0..=512.0).contains(&self.font_size) {
            return Err(MediaError::Img(format!(
                "font_size 须 8..=512，实际 {}",
                self.font_size
            )));
        }
        if self.margin < 0 {
            return Err(MediaError::Img(format!(
                "margin 须 ≥ 0，实际 {}",
                self.margin
            )));
        }
        if (self.margin as i64) * 2 >= (w.min(h) as i64) {
            return Err(MediaError::Img(format!(
                "margin {} 过大（图 {w}x{h}）",
                self.margin
            )));
        }
        Ok(())
    }
}

/// 覆盖度估算：字形包围盒面积 / 画布面积（验收断言「确实画上东西」用，也随
/// 产物输出到 evidence 便于审计）。
pub fn coverage(text: &str, font_size: f32) -> f64 {
    let font = load_font();
    let scale = PxScale::from(font_size);
    let mut area = 0f64;
    for c in text.chars() {
        if let Some(g) = glyph_of(&font, c, scale, 0.0) {
            if let Some(og) = font.outline_glyph(g) {
                let b = og.px_bounds();
                if b.width() > 0.0 && b.height() > 0.0 {
                    area += (b.width() * b.height()) as f64;
                }
            }
        }
    }
    area
}

fn load_font() -> FontArc {
    FontArc::try_from_vec(FONT_BYTES.to_vec()).expect("内嵌字体必须可解析（sha 锁定）")
}

fn glyph_of(font: &FontArc, c: char, scale: PxScale, x: f32) -> Option<Glyph> {
    let id = font.glyph_id(c);
    // GlyphId(0) = notdef：空格等空白字符除外（Noto 无 notdef 覆盖时才落 0）
    if id.0 == 0 && !c.is_whitespace() {
        return None;
    }
    Some(Glyph {
        id,
        scale,
        position: Point { x, y: 0.0 },
    })
}

/// 在 `src` 上叠文字水印，结果写 `dest`（格式随 dest 扩展名）。
/// 返回 (宽, 高)（尺寸不变，水印是叠加不是缩放）。
pub fn watermark_text(src: &Path, dest: &Path, wm: &TextWatermark) -> Result<(i32, i32)> {
    let (w, h, mut px) = crate::rgb8(src)?;
    wm.validate(w, h)?;
    let font = load_font();
    let scale = PxScale::from(wm.font_size);
    let scaled = PxScaleFont {
        font: font.clone(),
        scale,
    };

    // 第一遍：推进笔位 + 联合包围盒（基线系，y=0 为基线）
    let mut pen = 0f32;
    let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
    let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
    let mut laid: Vec<(ab_glyph::OutlinedGlyph, [f32; 2])> = Vec::new();
    for c in wm.text.chars() {
        let Some(g) = glyph_of(&font, c, scale, pen) else {
            return Err(MediaError::Img(format!(
                "水印文字含字体不支持的字符: '{c}'（U+{:04X}）",
                c as u32
            )));
        };
        let adv = scaled.h_advance(g.id);
        if let Some(og) = font.outline_glyph(g) {
            let b = og.px_bounds();
            if b.width() > 0.0 && b.height() > 0.0 {
                min_x = min_x.min(b.min.x);
                min_y = min_y.min(b.min.y);
                max_x = max_x.max(b.max.x);
                max_y = max_y.max(b.max.y);
                laid.push((og, [b.min.x, b.min.y]));
            }
        }
        pen += adv;
    }
    if laid.is_empty() {
        return Err(MediaError::Img("水印文字全是空白（拒绝空产物）".into()));
    }
    let bw = (max_x - min_x) as i32;
    let bh = (max_y - min_y) as i32;
    if bw > w || bh > h {
        return Err(MediaError::Img(format!(
            "水印尺寸 {bw}x{bh} 超过图面 {w}x{h}（font_size={}/text_len={}）",
            wm.font_size,
            wm.text.chars().count()
        )));
    }
    // 落位：包围盒左上角
    let (avail_x, avail_y) = ((w - bw).max(0), (h - bh).max(0));
    let m = wm.margin;
    let (ox, oy) = match wm.position {
        WatermarkPosition::TopLeft => (m, m),
        WatermarkPosition::TopRight => (avail_x - m, m),
        WatermarkPosition::Center => (avail_x / 2, avail_y / 2),
        WatermarkPosition::BottomLeft => (m, avail_y - m),
        WatermarkPosition::BottomRight => (avail_x - m, avail_y - m),
    };

    // 第二遍：光栅化混合（coverage × opacity）
    let alpha_base = f32::from(wm.opacity) / 100.0;
    let uw = w as u32;
    for (og, gmin) in laid {
        let gx = (ox as f32 + (gmin[0] - min_x)).round() as i32;
        let gy = (oy as f32 + (gmin[1] - min_y)).round() as i32;
        og.draw(|x, y, v| {
            let ix = gx + x as i32;
            let iy = gy + y as i32;
            if ix < 0 || iy < 0 || ix >= w || iy >= h {
                return; // 钳制式防御（理论上包围盒已保证不越界）
            }
            let a = v * alpha_base;
            let i = (iy as u32 * uw + ix as u32) as usize * 3;
            for ch in 0..3 {
                let d = px[i + ch] as f32;
                px[i + ch] = (d * (1.0 - a) + wm.color[ch] as f32 * a).round() as u8;
            }
        });
    }
    encode_rgb_to(w, h, &px, dest, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dam-wm-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_src(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let p = dir.join(name);
        let img = image::RgbImage::from_fn(w, h, |_, _| image::Rgb([240, 240, 240]));
        image::DynamicImage::ImageRgb8(img)
            .save_with_format(&p, image::ImageFormat::Jpeg)
            .unwrap();
        p
    }

    fn count_colored(path: &Path) -> (usize, usize) {
        let (w, h, px) = crate::rgb8(path).unwrap();
        let mut n = 0usize;
        for pxs in px.chunks(3) {
            // 与底色 240 差异明显的像素 = 被水印触及
            if pxs.iter().any(|&c| c < 200) {
                n += 1;
            }
        }
        (n, (w * h) as usize)
    }

    #[test]
    fn watermark_draws_all_positions_and_supports_cjk() {
        let dir = tmp_dir("pos");
        let src = write_src(&dir, "src.jpg", 200, 100);
        for (pos, _) in [
            ("top_left", 0),
            ("top_right", 0),
            ("center", 0),
            ("bottom_left", 0),
            ("bottom_right", 0),
        ] {
            let dest = dir.join(format!("out-{pos}.jpg"));
            let wm = TextWatermark {
                text: "测试Watermark".into(),
                position: WatermarkPosition::parse(pos).unwrap(),
                opacity: 80,
                font_size: 24.0,
                margin: 8,
                color: [10, 20, 30],
            };
            let (w, h) = watermark_text(&src, &dest, &wm).unwrap();
            assert_eq!((w, h), (200, 100));
            let (colored, total) = count_colored(&dest);
            assert!(colored > 30, "{pos}: 水印未落墨 colored={colored}");
            assert!(
                colored * 10 < total,
                "{pos}: 水印覆盖异常大 colored={colored}/{total}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opacity_scales_coverage_darkness() {
        let dir = tmp_dir("op");
        let src = write_src(&dir, "src.jpg", 200, 100);
        let mut avg = Vec::new();
        for op in [20u8, 100] {
            let dest = dir.join(format!("o{op}.jpg"));
            watermark_text(
                &src,
                &dest,
                &TextWatermark {
                    text: "水印".into(),
                    position: WatermarkPosition::Center,
                    opacity: op,
                    font_size: 40.0,
                    margin: 4,
                    color: [0, 0, 0],
                },
            )
            .unwrap();
            let (w, h, px) = crate::rgb8(&dest).unwrap();
            let sum: u64 = px.iter().map(|&c| c as u64).sum();
            avg.push(sum as f64 / ((w * h * 3) as f64));
        }
        assert!(
            avg[0] > avg[1],
            "低透明度({})应比不透明({})更接近底色",
            avg[0],
            avg[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_params_fail_loudly() {
        let dir = tmp_dir("bad");
        let src = write_src(&dir, "src.jpg", 60, 40);
        let mk = |text: &str, fs: f32, op: u8| TextWatermark {
            text: text.into(),
            position: WatermarkPosition::Center,
            opacity: op,
            font_size: fs,
            margin: 2,
            color: [0, 0, 0],
        };
        // emoji 缺字形
        let dest = dir.join("e1.jpg");
        assert!(watermark_text(&src, &dest, &mk("ok😀", 16.0, 50)).is_err());
        // 空文本 / 纯空白
        assert!(watermark_text(&src, &dest, &mk("", 16.0, 50)).is_err());
        assert!(watermark_text(&src, &dest, &mk("  ", 16.0, 50)).is_err());
        // 字号越界
        assert!(watermark_text(&src, &dest, &mk("字", 4.0, 50)).is_err());
        // opacity 0
        assert!(watermark_text(&src, &dest, &mk("字", 16.0, 0)).is_err());
        // 文字大于画布
        assert!(watermark_text(&src, &dest, &mk("超长文字水印", 64.0, 50)).is_err());
        // 颜色解析
        assert!(TextWatermark::parse_color("ffffff").is_ok());
        assert!(TextWatermark::parse_color("fff").is_err());
        assert!(TextWatermark::parse_color("zzzzzz").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_estimates_nonzero() {
        assert!(coverage("测试ABC", 32.0) > 100.0);
        assert_eq!(coverage("", 32.0), 0.0);
    }
}
