//! DM-C1（t_d5c6c6a2）：inpaint 重绘区域的三态决议 + VL bbox 与 OCR 文本行框的交叉验证。
//!
//! ## 修的是什么
//!
//! 修前：VL 判「有水印」后返回 bbox，编排器**只要盒子非空就直接当重绘区域**——VL 幻视一个
//! 盒子（或把整幅商品图当水印）→ lama 把那块真商品像素重画掉，用户看到的产物比原图更差，
//! 而判定链一路记「已修复 + 复检」。区域没有任何本地证据复核。
//!
//! 修后（本模块的纯逻辑，编排器只负责取证据/写产物）：
//!
//! | 通道 | 区域来源 | 复核判据 |
//! |---|---|---|
//! | `explicit` | 包声明的 `box` / `mask` | 无需复核（作者显式给的） |
//! | VL bbox（默认） | VL `regions` / `where` | 与 OCR 行框重叠 > 50% → 用 **OCR 框**；≤ 50% 且该图有 OCR 文本 → 拒绝（`vl_region_mismatch`）；该图无 OCR 文本 → 接受但标 `vl_region_unverified` |
//! | `ocr_text` | 本地 OCR 行框（union） | 无框 → 回落 u2net |
//! | `u2net` | u2net 主体掩码 | 掩码面积 > 全图 40% → 拒绝（`region_too_large`） |
//!
//! ## 阈值为什么是 0.5 / 0.4
//!
//! * **0.5（重叠率）**：分子 = 交集面积，分母 = **VL 框自身面积**（不是 IoU）——判的是
//!   「VL 指的那块地方里有多少真的被 OCR 检出有文本」。用 IoU 会把「VL 框比文本框大一圈」
//!   误判成不重叠（水印框通常就比行框松），那不是我们要拒的情形。
//! * **0.4（掩码面积上限）**：重绘面积过半时产物已经不是「修掉水印」而是「重画一张图」；
//!   0.4 给「主体带大面积水印」留余量，同时把「u2net 把整幅商品图当主体」挡在门外。
//!
//! ## 与复检的关系（K3 报告 §6）
//!
//! 复检调用的是**同一个修复产物**、同一批 VL ask —— 区域换了，复检看到的仍然是「有没有
//! 水印」。所以区域决议必须落进 evidence（`region.source` / `region.boxes`），报告里才能
//! 说清「这次重绘的到底是哪块、凭什么」（见 reports/dm-c1-inpaint-wire.md）。

use serde_json::json;

/// VL bbox 与 OCR 行框的最小重叠率（分母 = VL 框面积）。
pub const VL_OCR_OVERLAP_MIN: f64 = 0.5;
/// u2net 掩码面积占全图的上限（超过 = 不是「修一小块」而是重画）。
pub const MASK_AREA_MAX: f64 = 0.4;

/// 重绘区域来源（落进 evidence.region.source，报告/审计的唯一口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionSource {
    /// 包声明显式给了 box/mask
    Explicit,
    /// VL bbox，且与 OCR 行框对得上（用 OCR 框）
    VlVerified,
    /// VL bbox，该图没有 OCR 文本可对照（接受 + warning）
    VlUnverified,
    /// OCR 行框（`region: ocr_text`）
    OcrText,
    /// u2net 主体掩码（`region: u2net` / ocr_text 回落）
    U2Net,
}

impl RegionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RegionSource::Explicit => "explicit",
            RegionSource::VlVerified => "vl_bbox_verified",
            RegionSource::VlUnverified => "vl_bbox_unverified",
            RegionSource::OcrText => "ocr_text",
            RegionSource::U2Net => "u2net",
        }
    }
}

/// 包的 `region` 声明（inpaint fixer params）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionMode {
    /// 只认声明的 box/mask（没有就转人工，不许 VL 兜）
    Explicit,
    /// OCR 文本行框（无框回落 u2net）
    OcrText,
    /// u2net 主体掩码
    U2Net,
}

impl RegionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "explicit" => Some(RegionMode::Explicit),
            "ocr_text" => Some(RegionMode::OcrText),
            "u2net" => Some(RegionMode::U2Net),
            _ => None,
        }
    }
}

/// 区域决议结果（编排器据此注入 box / 写 mask）。
#[derive(Debug, Clone, PartialEq)]
pub struct RegionPlan {
    pub source: RegionSource,
    /// 归一化矩形（1 个 → inpaint 的 `box`；多个 → 合成 mask）
    pub regions: Vec<[f64; 4]>,
    /// VL 原始返回（审计：拒绝时也要能看见模型说了什么）
    pub vl_regions: Vec<[f64; 4]>,
    /// 本图的 OCR 行框（审计）
    pub ocr_boxes: Vec<[f64; 4]>,
    /// VL 框与 OCR 框的最大重叠率（None = 没得比：无 VL 框或无 OCR 框）
    pub overlap: Option<f64>,
    /// 区域可信度提示（`vl_region_unverified` = 无 OCR 可对照）
    pub warning: Option<&'static str>,
}

impl RegionPlan {
    /// evidence.region 片段（每条 VL 修复行都带，审计与报告的唯一来源）。
    pub fn evidence(&self) -> serde_json::Value {
        json!({
            "source": self.source.as_str(),
            "boxes": self.regions,
            "vlRegions": self.vl_regions,
            "ocrBoxes": self.ocr_boxes,
            "overlap": self.overlap,
            "warning": self.warning,
        })
    }
}

/// 区域决议拒绝（→ 编排器落 undetermined，reason 直接给用户看）。
#[derive(Debug, Clone, PartialEq)]
pub enum RegionRefusal {
    /// 没有任何可用区域（VL 没给框 / 无 OCR 框 / u2net 不可用）
    NoRegion,
    /// 该图有 OCR 文本，但 VL 框和它们都重叠不过半 —— VL 指的「不存在的东西」
    Mismatch { overlap: f64 },
    /// 区域太大（掩码面积占全图超上限）—— 重绘 = 重画，拒绝
    TooLarge { ratio: f64 },
}

impl RegionRefusal {
    pub fn reason(&self) -> &'static str {
        match self {
            RegionRefusal::NoRegion => "vl_no_region",
            RegionRefusal::Mismatch { .. } => "vl_region_mismatch",
            RegionRefusal::TooLarge { .. } => "region_too_large",
        }
    }

    pub fn evidence(&self) -> serde_json::Value {
        match self {
            RegionRefusal::NoRegion => json!({ "reason": self.reason() }),
            RegionRefusal::Mismatch { overlap } => json!({
                "reason": self.reason(),
                "overlap": overlap,
                "threshold": VL_OCR_OVERLAP_MIN,
            }),
            RegionRefusal::TooLarge { ratio } => json!({
                "reason": self.reason(),
                "areaRatio": ratio,
                "maxRatio": MASK_AREA_MAX,
            }),
        }
    }
}

fn clamp01(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 归一化化矩形（消费外部输入：clamp + 保证 x0<x1 / y0<y1；退化 → None）。
pub fn normalize_rect(r: &[f64; 4]) -> Option<[f64; 4]> {
    let x0 = clamp01(r[0]);
    let y0 = clamp01(r[1]);
    let x1 = clamp01(r[2]);
    let y1 = clamp01(r[3]);
    let (x0, x1) = if x1 < x0 { (x1, x0) } else { (x0, x1) };
    let (y0, y1) = if y1 < y0 { (y1, y0) } else { (y0, y1) };
    if x1 - x0 <= f64::EPSILON || y1 - y0 <= f64::EPSILON {
        return None;
    }
    Some([x0, y0, x1, y1])
}

fn area(r: &[f64; 4]) -> f64 {
    (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0)
}

/// VL 框里有多大比例落在 OCR 行框内（分母 = VL 框面积，clamp 0..1）。
///
/// 多个 OCR 框时取并集意义上的覆盖：逐个求交集后**合并**（简单相加并 clamp，行框通常不重叠，
/// 相加即并集；重叠时 clamp 到 1.0 仍然是「全覆盖」，判定方向不受影响）。
pub fn overlap_ratio(vl: &[f64; 4], ocr: &[[f64; 4]]) -> f64 {
    let va = area(vl);
    if va <= f64::EPSILON {
        return 0.0;
    }
    let mut covered = 0.0;
    for b in ocr {
        let iw = (vl[2].min(b[2]) - vl[0].max(b[0])).max(0.0);
        let ih = (vl[3].min(b[3]) - vl[1].max(b[1])).max(0.0);
        covered += iw * ih;
    }
    (covered / va).clamp(0.0, 1.0)
}

/// 单矩形面积占全图比例（0..1）。
pub fn rect_area_ratio(r: &[f64; 4]) -> f64 {
    area(r).clamp(0.0, 1.0)
}

/// 掩码（0/非 0 字节）面积占比。
pub fn mask_area_ratio(mask: &[u8]) -> f64 {
    if mask.is_empty() {
        return 0.0;
    }
    let on = mask.iter().filter(|b| **b != 0).count();
    on as f64 / mask.len() as f64
}

/// 区域整体面积占比（多矩形按并集近似：逐个面积相加 clamp；只用于「是不是太大」的判断，
/// 保守取上界）。
pub fn regions_area_ratio(regions: &[[f64; 4]]) -> f64 {
    regions
        .iter()
        .map(|r| area(r))
        .sum::<f64>()
        .clamp(0.0, 1.0)
}

/// VL bbox 通道：与 OCR 行框交叉验证（默认路径，`region` 未声明时的现行行为 + 复核）。
pub fn plan_vl_bbox(vl_regions: &[[f64; 4]], ocr_boxes: &[[f64; 4]]) -> Result<RegionPlan, RegionRefusal> {
    let vl: Vec<[f64; 4]> = vl_regions.iter().filter_map(normalize_rect).collect();
    let ocr: Vec<[f64; 4]> = ocr_boxes.iter().filter_map(normalize_rect).collect();
    if vl.is_empty() {
        return Err(RegionRefusal::NoRegion);
    }
    if ocr.is_empty() {
        // 无 OCR 文本可对照：接受 VL 区域但必须留 warning（不许当成「已验证」）
        return Ok(RegionPlan {
            source: RegionSource::VlUnverified,
            regions: vl.clone(),
            vl_regions: vl,
            ocr_boxes: ocr,
            overlap: None,
            warning: Some("vl_region_unverified"),
        });
    }
    // 每个 VL 框都找支撑它的 OCR 框；只要有一个 VL 框无支撑 → 整次拒绝。
    let mut supported: Vec<[f64; 4]> = Vec::new();
    let mut best = 0.0_f64;
    for v in &vl {
        let ratios: Vec<f64> = ocr.iter().map(|b| overlap_ratio(v, std::slice::from_ref(b))).collect();
        let top = ratios.iter().copied().fold(0.0_f64, f64::max);
        best = best.max(top);
        if top <= VL_OCR_OVERLAP_MIN {
            return Err(RegionRefusal::Mismatch { overlap: top });
        }
        for (b, r) in ocr.iter().zip(ratios) {
            if r > VL_OCR_OVERLAP_MIN && !supported.contains(b) {
                supported.push(*b);
            }
        }
    }
    Ok(RegionPlan {
        source: RegionSource::VlVerified,
        regions: supported,
        vl_regions: vl,
        ocr_boxes: ocr,
        overlap: Some(best),
        warning: None,
    })
}

/// `region: ocr_text` 通道：直接取 OCR 行框（无框 → NoRegion，调用方回落 u2net）。
pub fn plan_ocr_text(ocr_boxes: &[[f64; 4]]) -> Result<RegionPlan, RegionRefusal> {
    let ocr: Vec<[f64; 4]> = ocr_boxes.iter().filter_map(normalize_rect).collect();
    if ocr.is_empty() {
        return Err(RegionRefusal::NoRegion);
    }
    Ok(RegionPlan {
        source: RegionSource::OcrText,
        regions: ocr.clone(),
        vl_regions: Vec::new(),
        ocr_boxes: ocr,
        overlap: None,
        warning: None,
    })
}

/// `region: u2net` 通道（也给 ocr_text 的回落用）：主体掩码 + 面积上限。
pub fn plan_u2net(regions: &[[f64; 4]], mask_ratio: f64) -> Result<RegionPlan, RegionRefusal> {
    let rects: Vec<[f64; 4]> = regions.iter().filter_map(normalize_rect).collect();
    if rects.is_empty() {
        return Err(RegionRefusal::NoRegion);
    }
    if mask_ratio > MASK_AREA_MAX {
        return Err(RegionRefusal::TooLarge { ratio: mask_ratio });
    }
    Ok(RegionPlan {
        source: RegionSource::U2Net,
        regions: rects,
        vl_regions: Vec::new(),
        ocr_boxes: Vec::new(),
        overlap: None,
        warning: None,
    })
}

/// 包声明的显式区域（box/mask）：只做记录，不改注入值。
pub fn plan_explicit(regions: Vec<[f64; 4]>) -> RegionPlan {
    RegionPlan {
        source: RegionSource::Explicit,
        regions,
        vl_regions: Vec::new(),
        ocr_boxes: Vec::new(),
        overlap: None,
        warning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 合成 OCR 框 vs VL bbox 的三种重叠情形（卡内验收要求） ----------

    #[test]
    fn overlap_gt_half_uses_ocr_box_not_vl_box() {
        // 水印框通常比行框松：VL 给 [0.10,0.10,0.40,0.20]，OCR 行框是它中间那条
        let vl = [0.10, 0.10, 0.40, 0.20];
        let ocr = [[0.12, 0.12, 0.38, 0.18]];
        let r = overlap_ratio(&vl, &ocr);
        assert!(r > VL_OCR_OVERLAP_MIN, "重叠率 {r} 应过半");
        let plan = plan_vl_bbox(&[vl], &ocr).expect("有文本支撑");
        assert_eq!(plan.source, RegionSource::VlVerified);
        assert_eq!(plan.regions, ocr.to_vec(), "用 OCR 框，而不是 VL 框");
        assert!(plan.warning.is_none());
    }

    #[test]
    fn overlap_le_half_is_refused_as_mismatch() {
        // VL 指左上，OCR 文本在右下 → 重叠 0
        let vl = [0.05, 0.05, 0.25, 0.20];
        let ocr = [[0.60, 0.60, 0.90, 0.80]];
        match plan_vl_bbox(&[vl], &ocr) {
            Err(RegionRefusal::Mismatch { overlap }) => {
                assert!(overlap < VL_OCR_OVERLAP_MIN);
                assert_eq!(RegionRefusal::Mismatch { overlap }.reason(), "vl_region_mismatch");
            }
            other => panic!("应拒绝，实际 {other:?}"),
        }
        // 边缘情形：重叠恰好 = 0.5 → 不达标（阈值是「> 50%」）
        let vl = [0.0, 0.0, 0.40, 0.20];
        let ocr = [[0.0, 0.0, 0.20, 0.20]];
        assert!((overlap_ratio(&vl, &[ocr[0]]) - 0.5).abs() < 1e-9);
        assert!(matches!(
            plan_vl_bbox(&[vl], &ocr),
            Err(RegionRefusal::Mismatch { .. })
        ));
    }

    #[test]
    fn no_ocr_text_accepts_vl_bbox_with_warning() {
        let vl = [0.1, 0.1, 0.3, 0.2];
        let plan = plan_vl_bbox(&[vl], &[]).expect("无 OCR 时不拒");
        assert_eq!(plan.source, RegionSource::VlUnverified);
        assert_eq!(plan.regions, vec![vl]);
        assert_eq!(plan.warning, Some("vl_region_unverified"));
        let ev = plan.evidence();
        assert_eq!(ev["warning"], "vl_region_unverified");
        assert_eq!(ev["source"], "vl_bbox_unverified");
    }

    #[test]
    fn empty_vl_region_is_refused() {
        assert_eq!(plan_vl_bbox(&[], &[]).unwrap_err(), RegionRefusal::NoRegion);
        // 退化矩形（零面积）等同没给
        assert_eq!(
            plan_vl_bbox(&[[0.2, 0.2, 0.2, 0.3]], &[]).unwrap_err(),
            RegionRefusal::NoRegion
        );
    }

    // ---------- u2net 40% 面积闸 ----------

    #[test]
    fn u2net_mask_over_40pct_is_refused() {
        let regions = [[0.0, 0.0, 1.0, 1.0]];
        match plan_u2net(&regions, 0.85) {
            Err(RegionRefusal::TooLarge { ratio }) => {
                assert!((ratio - 0.85).abs() < 1e-9);
            }
            other => panic!("应拒绝大面积，实际 {other:?}"),
        }
        // 40% 整不拒（阈值是「> 40%」）
        assert!(plan_u2net(&regions, MASK_AREA_MAX).is_ok());
        assert_eq!(
            plan_u2net(&[[0.1, 0.1, 0.3, 0.3]], 0.9).unwrap_err().reason(),
            "region_too_large"
        );
    }

    #[test]
    fn mask_area_ratio_counts_nonzero() {
        let mut mask = vec![0u8; 100];
        assert_eq!(mask_area_ratio(&mask), 0.0);
        for b in mask.iter_mut().take(40) {
            *b = 1;
        }
        assert!((mask_area_ratio(&mask) - 0.4).abs() < 1e-9);
        assert_eq!(mask_area_ratio(&[]), 0.0);
    }

    // ---------- ocr_text 通道 ----------

    #[test]
    fn ocr_text_takes_all_boxes_and_empty_falls_back() {
        let boxes = [[0.1, 0.1, 0.3, 0.2], [0.1, 0.5, 0.6, 0.6]];
        let plan = plan_ocr_text(&boxes).expect("有框");
        assert_eq!(plan.source, RegionSource::OcrText);
        assert_eq!(plan.regions.len(), 2);
        assert_eq!(plan_ocr_text(&[]).unwrap_err(), RegionRefusal::NoRegion, "无框 → 调用方回落 u2net");
    }

    #[test]
    fn mode_parse_accepts_three_states_only() {
        assert_eq!(RegionMode::parse("explicit"), Some(RegionMode::Explicit));
        assert_eq!(RegionMode::parse(" OCR_TEXT "), Some(RegionMode::OcrText));
        assert_eq!(RegionMode::parse("u2net"), Some(RegionMode::U2Net));
        assert_eq!(RegionMode::parse("auto"), None, "未知取值由调用方响亮拒绝");
    }

    #[test]
    fn normalize_rect_swaps_and_clamps() {
        assert_eq!(normalize_rect(&[0.4, 0.2, 0.1, 0.1]), Some([0.1, 0.1, 0.4, 0.2]));
        assert_eq!(normalize_rect(&[-1.0, 0.0, 2.0, 1.0]), Some([0.0, 0.0, 1.0, 1.0]));
        // NaN 坐标按 0 处理（clamp01），剩下的仍是合法矩形 —— 不是「整框丢弃」
        assert_eq!(normalize_rect(&[f64::NAN, 0.0, 0.5, 0.5]), Some([0.0, 0.0, 0.5, 0.5]));
        // 真退化（零面积）才是 None
        assert_eq!(normalize_rect(&[0.3, 0.3, 0.3, 0.5]), None);
    }
}
