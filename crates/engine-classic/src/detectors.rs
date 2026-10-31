//! C1 四个 classic 检测器（PLAN.md §6.4 / 阶段排期 C1）：纯 Rust 函数，毫秒级、
//! 确定性、可审计复现。输入一律是内存缓冲/尺寸元数据（文件解码由上游 core-media
//! 完成）——本模块不碰文件系统、不碰模型，同输入必得同输出（复现性铁律）。
//!
//! 检测器与阈值语义（与电商处理插件的 `resources/rules.json` 阈值对齐）：
//! - `size_min`   尺寸达标：min(宽,高) >= params.min（元数据档，不解码图）
//! - `white_edge` 白底纯净度：边缘带像素与纯白的最差通道偏差的均值 <= params.max_dev
//! - `sharpness`  清晰度：灰度拉普拉斯响应方差 >= params.min（方差大 = 边缘锐）
//! - `exposure`   曝光：死黑/死白合计占比 <= params.max_dead_ratio，均值亮度在区间内

use serde::Deserialize;

/// Rec.709 加权灰度（与 `image` crate `to_luma8` 同系数，整型近似截断）。
/// luma 类检测器（sharpness/exposure）内部用它从 RGB 输入转换。
pub fn luma_from_rgb(rgb: &[u8]) -> Vec<u8> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|px| ((2126 * px[0] as u32 + 7152 * px[1] as u32 + 722 * px[2] as u32) / 10_000) as u8)
        .collect()
}

// ---------- size_min ----------

/// `size_min` 参数。deny_unknown_fields：写错的参数键当场报错，不允许静默忽略
/// （阈值拼错的判定报告无法审计）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeMinParams {
    /// 最短边下限（px）。
    pub min: u32,
}

/// `size_min` 结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SizeReport {
    pub min_side: u32,
    pub required: u32,
    pub pass: bool,
}

/// 尺寸达标：min(宽,高) >= min。元数据档输入（DB 宽高字段），不解码图。
pub fn size_min(width: u32, height: u32, params: &SizeMinParams) -> SizeReport {
    let min_side = width.min(height);
    SizeReport {
        min_side,
        required: params.min,
        pass: min_side >= params.min,
    }
}

// ---------- white_edge ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhiteEdgeParams {
    /// 边缘带均值偏差上限（0-255 尺度）。mean_dev <= max_dev 判白底合格。
    pub max_dev: f64,
    /// 边缘带宽度（像素），默认 4。
    #[serde(default = "default_band")]
    pub band: u32,
}

fn default_band() -> u32 {
    4
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhiteEdgeReport {
    /// 边缘带均值偏差（score，越小越纯白）。
    pub mean_dev: f64,
    /// 边缘带最差单像素偏差。
    pub max_dev: f64,
    /// 实际使用的带宽（小图会被钳到短边一半）。
    pub band: u32,
    pub edge_pixels: u64,
    pub pass: bool,
}

/// 白底纯净度：外圈 `band` 像素带内，每像素取与纯白的最差通道偏差
/// （255 - min(R,G,B)，对色偏比单通道均值更敏感），返回全带均值。
/// 白底商品图应接近 0（<= max_dev 合格）。中心主体不参与统计。
pub fn white_edge(
    rgb: &[u8],
    width: u32,
    height: u32,
    params: &WhiteEdgeParams,
) -> Result<WhiteEdgeReport, String> {
    if width == 0 || height == 0 {
        return Err(format!("{width}x{height} 空图"));
    }
    if rgb.len() != width as usize * height as usize * 3 {
        return Err(format!(
            "像素缓冲长度 {} 与 {width}x{height}x3 不符",
            rgb.len()
        ));
    }
    // band 至少 1，至多短边一半（band >= 短边时整幅都算边缘，避免空带/除零）
    let band = params.band.max(1).min(width.min(height).div_ceil(2)) as usize;
    let w = width as usize;
    let h = height as usize;
    let mut sum: u64 = 0;
    let mut worst: u8 = 0;
    let mut count: u64 = 0;
    for y in 0..h {
        for x in 0..w {
            let on_edge = x < band || x >= w - band || y < band || y >= h - band;
            if !on_edge {
                continue;
            }
            let px = &rgb[(y * w + x) * 3..(y * w + x) * 3 + 3];
            let dev = 255u16 - px.iter().map(|&c| c as u16).min().unwrap();
            sum += dev as u64;
            worst = worst.max(dev as u8);
            count += 1;
        }
    }
    let mean_dev = sum as f64 / count as f64;
    Ok(WhiteEdgeReport {
        mean_dev,
        max_dev: worst as f64,
        band: band as u32,
        edge_pixels: count,
        pass: mean_dev <= params.max_dev,
    })
}

// ---------- sharpness ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharpnessParams {
    /// 拉普拉斯方差下限（0-255 像素值尺度）。variance >= min 判清晰。
    pub min: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SharpnessReport {
    pub variance: f64,
    pub required_min: f64,
    pub pixels: u64,
    pub pass: bool,
}

/// 清晰度：灰度拉普拉斯核 `[0,1,0;1,-4,1;0,1,0]` 响应的方差（只算非边界像素）。
/// 模糊图响应趋零、方差小；清晰边缘方差大。图不足 3x3 报错（无法判锐度）。
pub fn sharpness_laplacian_variance(
    luma: &[u8],
    width: u32,
    height: u32,
) -> Result<(f64, u64), String> {
    if width < 3 || height < 3 {
        return Err(format!("{width}x{height} 太小，拉普拉斯需 >= 3x3"));
    }
    if luma.len() != width as usize * height as usize {
        return Err(format!("luma 长度 {} 与 {width}x{height} 不符", luma.len()));
    }
    let w = width as usize;
    let h = height as usize;
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let lap =
                luma[i - 1] as i64 + luma[i + 1] as i64 + luma[i - w] as i64 + luma[i + w] as i64
                    - 4 * luma[i] as i64;
            let v = lap as f64;
            sum += v;
            sum_sq += v * v;
            n += 1;
        }
    }
    let mean = sum / n as f64;
    let variance = sum_sq / n as f64 - mean * mean;
    Ok((variance, n))
}

// ---------- exposure ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposureParams {
    /// 死黑/死白合计占比上限。<= max_dead_ratio 且均值亮度在区间内才合格。
    pub max_dead_ratio: f64,
    /// luma <= black_cut 记死黑，默认 10。
    #[serde(default = "default_black_cut")]
    pub black_cut: u8,
    /// luma >= 255 - white_cut 记死白，默认 10。
    #[serde(default = "default_white_cut")]
    pub white_cut: u8,
    /// 均值亮度下限（0-255），默认 0（不设限）。
    #[serde(default)]
    pub min_mean: f64,
    /// 均值亮度上限（0-255），默认 255（不设限）。
    #[serde(default = "default_max_mean")]
    pub max_mean: f64,
}

fn default_black_cut() -> u8 {
    10
}

fn default_white_cut() -> u8 {
    10
}

fn default_max_mean() -> f64 {
    255.0
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExposureReport {
    /// 死黑+死白合计占比（score，越小越均衡）。
    pub dead_ratio: f64,
    pub dead_black_ratio: f64,
    pub dead_white_ratio: f64,
    /// 均值亮度 0-255。
    pub mean: f64,
    pub pass: bool,
}

/// 曝光：直方图两端死区占比 + 均值亮度。含边界：luma == black_cut 记死黑、
/// luma == 255 - white_cut 记死白（阈值语义「压线即死区」，报告可复现）。
pub fn exposure(
    luma: &[u8],
    width: u32,
    height: u32,
    params: &ExposureParams,
) -> Result<ExposureReport, String> {
    if width == 0 || height == 0 {
        return Err(format!("{width}x{height} 空图"));
    }
    if luma.len() != width as usize * height as usize {
        return Err(format!("luma 长度 {} 与 {width}x{height} 不符", luma.len()));
    }
    let n = luma.len() as f64;
    let white_cut_line = 255u16 - params.white_cut as u16;
    let mut dead_black = 0u64;
    let mut dead_white = 0u64;
    let mut sum = 0u64;
    for &v in luma {
        sum += v as u64;
        if v <= params.black_cut {
            dead_black += 1;
        }
        if v as u16 >= white_cut_line {
            dead_white += 1;
        }
    }
    let dead_black_ratio = dead_black as f64 / n;
    let dead_white_ratio = dead_white as f64 / n;
    let mean = sum as f64 / n;
    Ok(ExposureReport {
        dead_ratio: dead_black_ratio + dead_white_ratio,
        dead_black_ratio,
        dead_white_ratio,
        mean,
        pass: dead_black_ratio + dead_white_ratio <= params.max_dead_ratio
            && mean >= params.min_mean
            && mean <= params.max_mean,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- size_min：边界（min == min_side 过线即合格） ----------

    #[test]
    fn size_min_boundary_and_ratio() {
        let p = SizeMinParams { min: 700 };
        // 正方形压线：700x700 vs min=700 → 合格
        assert!(size_min(700, 700, &p).pass);
        // 699 差 1px → 不合格
        assert!(!size_min(699, 1000, &p).pass);
        // 竖图按短边算：1000x699 → 不合格
        assert_eq!(size_min(1000, 699, &p).min_side, 699);
        assert!(!size_min(1000, 699, &p).pass);
        // 宽松阈值 → 合格
        assert!(size_min(800, 600, &SizeMinParams { min: 600 }).pass);
        assert!(!size_min(800, 600, &SizeMinParams { min: 601 }).pass);
    }

    // ---------- white_edge：合成白底 + 中心主体不计入 ----------

    #[test]
    fn white_edge_pure_white_is_zero() {
        // 10x10 纯白，band=4 → 全部 96 像素都是边缘带，偏差 0
        let rgb = vec![255u8; 10 * 10 * 3];
        let r = white_edge(
            &rgb,
            10,
            10,
            &WhiteEdgeParams {
                max_dev: 8.0,
                band: 4,
            },
        )
        .unwrap();
        assert_eq!(r.mean_dev, 0.0);
        assert_eq!(r.max_dev, 0.0);
        assert_eq!(r.edge_pixels, 96);
        assert!(r.pass);
    }

    #[test]
    fn white_edge_offwhite_border_pixel_and_center_ignored() {
        // 10x10：纯白底，(0,0) 一个 240（偏差 15），中心 2x2 涂黑（主体，不计入）
        let mut rgb = vec![255u8; 10 * 10 * 3];
        rgb[0..3].fill(240);
        for y in 4..6 {
            for x in 4..6 {
                let i = (y * 10 + x) * 3;
                rgb[i..i + 3].fill(0);
            }
        }
        let r = white_edge(
            &rgb,
            10,
            10,
            &WhiteEdgeParams {
                max_dev: 8.0,
                band: 4,
            },
        )
        .unwrap();
        // 边缘带 96 像素中 1 个偏差 15 → 均值 15/96
        assert!((r.mean_dev - 15.0 / 96.0).abs() < 1e-12);
        assert_eq!(r.max_dev, 15.0);
        assert!(r.pass, "15/96 < 8 依然合格");
        // 阈值卡到 0.1：均值 0.15625 > 0.1 → 不合格（阈值边界）
        let strict = white_edge(
            &rgb,
            10,
            10,
            &WhiteEdgeParams {
                max_dev: 0.1,
                band: 4,
            },
        )
        .unwrap();
        assert!(!strict.pass);
    }

    #[test]
    fn white_edge_band_clamped_and_len_check() {
        // band 超过短边一半 → 钳制，不 panic
        let rgb = vec![200u8; 4 * 4 * 3];
        let r = white_edge(
            &rgb,
            4,
            4,
            &WhiteEdgeParams {
                max_dev: 8.0,
                band: 100,
            },
        )
        .unwrap();
        assert_eq!(r.band, 2);
        assert_eq!(r.edge_pixels, 16);
        // 缓冲长度不符响亮报错
        assert!(white_edge(
            &[255u8; 10],
            10,
            10,
            &WhiteEdgeParams {
                max_dev: 8.0,
                band: 4
            }
        )
        .is_err());
    }

    // ---------- sharpness：均匀=0 / 3x3 精确值 / 太小报错 ----------

    #[test]
    fn sharpness_uniform_is_zero() {
        let luma = vec![128u8; 32 * 32];
        let (v, n) = sharpness_laplacian_variance(&luma, 32, 32).unwrap();
        assert_eq!(v, 0.0);
        assert_eq!(n, 30 * 30);
    }

    #[test]
    fn sharpness_exact_4x4_checkerboard() {
        // 4x4 棋盘格（(x+y) 偶=255）：4 个内点 lap = ±1020、均值 0 → 方差精确 = 1020²
        let mut luma = vec![0u8; 16];
        for y in 0..4usize {
            for x in 0..4usize {
                if (x + y) % 2 == 0 {
                    luma[y * 4 + x] = 255;
                }
            }
        }
        let (v, n) = sharpness_laplacian_variance(&luma, 4, 4).unwrap();
        assert_eq!(n, 4);
        assert!((v - 1020.0 * 1020.0).abs() < 1e-9, "v = {v}");
        // 3x3 单内点：单样本方差退化为 0（数学事实，记录语义）
        let mut luma = vec![255u8; 9];
        luma[4] = 0;
        let (v, n) = sharpness_laplacian_variance(&luma, 3, 3).unwrap();
        assert_eq!(n, 1);
        assert_eq!(v, 0.0);
    }

    #[test]
    fn sharpness_checkerboard_sharp_vs_blurred() {
        // 棋盘格（锐）方差远大于均匀渐变（糊）
        let mut sharp = vec![0u8; 16 * 16];
        for y in 0..16 {
            for x in 0..16 {
                sharp[y * 16 + x] = if (x + y) % 2 == 0 { 255 } else { 0 };
            }
        }
        let (vs, _) = sharpness_laplacian_variance(&sharp, 16, 16).unwrap();
        let mut blur = vec![0u8; 16 * 16];
        for y in 0..16 {
            for x in 0..16 {
                blur[y * 16 + x] = (x * 4) as u8; // 平滑渐变
            }
        }
        let (vb, _) = sharpness_laplacian_variance(&blur, 16, 16).unwrap();
        assert!(vs > 100_000.0);
        assert!(vb < 100.0);
        // 2x2 太小响亮报错
        assert!(sharpness_laplacian_variance(&[0u8; 4], 2, 2).is_err());
    }

    // ---------- exposure：死区边界（压线即死区）+ 均值区间 ----------

    #[test]
    fn exposure_dead_zone_boundaries() {
        // 1x4：luma=10（== black_cut → 死黑）、11（活）、245（== 255-white_cut → 死白）、246（活?）
        // 246 >= 245 也死 → 用 244/246 拆开验证两侧
        let luma = vec![10u8, 11, 244, 245];
        let r = exposure(
            &luma,
            4,
            1,
            &ExposureParams {
                max_dead_ratio: 0.5,
                black_cut: 10,
                white_cut: 10,
                min_mean: 0.0,
                max_mean: 255.0,
            },
        )
        .unwrap();
        assert_eq!(r.dead_black_ratio, 0.25); // 10 死、11 活
        assert_eq!(r.dead_white_ratio, 0.25); // 245 死、244 活
        assert_eq!(r.dead_ratio, 0.5);
        assert!(r.pass, "恰好压线 max_dead_ratio=0.5 → 合格");
        let strict = exposure(
            &luma,
            4,
            1,
            &ExposureParams {
                max_dead_ratio: 0.49,
                black_cut: 10,
                white_cut: 10,
                min_mean: 0.0,
                max_mean: 255.0,
            },
        )
        .unwrap();
        assert!(!strict.pass);
    }

    #[test]
    fn exposure_half_black_half_white_fails_mean_range() {
        // 10x10 前半黑后半白：死区 100%、均值 127.5
        let mut luma = vec![0u8; 50];
        luma.extend(vec![255u8; 50]);
        let r = exposure(
            &luma,
            10,
            10,
            &ExposureParams {
                max_dead_ratio: 0.02,
                black_cut: 10,
                white_cut: 10,
                min_mean: 0.0,
                max_mean: 255.0,
            },
        )
        .unwrap();
        assert!((r.mean - 127.5).abs() < 1e-12);
        assert_eq!(r.dead_ratio, 1.0);
        assert!(!r.pass);
        // 中灰全图：死区 0、均值区间内 → 合格；区间卡死 → 不合格
        let gray = vec![128u8; 100];
        let ok = exposure(
            &gray,
            10,
            10,
            &ExposureParams {
                max_dead_ratio: 0.02,
                black_cut: 10,
                white_cut: 10,
                min_mean: 100.0,
                max_mean: 150.0,
            },
        )
        .unwrap();
        assert!(ok.pass);
        let dark = exposure(
            &gray,
            10,
            10,
            &ExposureParams {
                max_dead_ratio: 0.02,
                black_cut: 10,
                white_cut: 10,
                min_mean: 200.0,
                max_mean: 255.0,
            },
        )
        .unwrap();
        assert!(!dark.pass);
    }

    // ---------- luma 转换 ----------

    #[test]
    fn luma_from_rgb_pure_colors() {
        assert_eq!(luma_from_rgb(&[255, 255, 255]), vec![255]);
        assert_eq!(luma_from_rgb(&[0, 0, 0]), vec![0]);
        assert_eq!(luma_from_rgb(&[255, 0, 0]), vec![54]); // 0.2126*255
        assert_eq!(luma_from_rgb(&[0, 255, 0]), vec![182]); // 0.7152*255
        assert_eq!(luma_from_rgb(&[0, 0, 255]), vec![18]); // 0.0722*255
        assert_eq!(luma_from_rgb(&[1, 2, 3, 4, 5, 6]).len(), 2);
    }
}
