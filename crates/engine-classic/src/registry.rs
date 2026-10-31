//! classic 检测器静态注册表（D12 裁决：进程内静态注册，编译进二进制）。
//! C2 编排器按 rules.yaml 的 `check.detector` 字符串查表分发；未知 id 显式报
//! 「未知检测器」，绝不静默跳过（判定可审计铁律）。加检测器 = detectors.rs 加
//! 函数 + 这里 DETECTORS 加一条。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::detectors;

/// 检测器消费的输入档（deferral chain 段位，PLAN.md §6.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// 元数据档：不解码图，直接吃 DB 宽高字段（~0ms）。
    Metadata,
    /// 像素档：解码一次的 RGB 交错缓冲（classic 段，毫秒级）。
    Rgb,
}

/// 检测器输入（注册表分发入口统一收这个枚举，输入档不符显式报错）。
#[derive(Debug, Clone)]
pub enum DetectorInput {
    Metadata {
        width: u32,
        height: u32,
    },
    Rgb {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

/// 检测输出：主分数 + 是否合格 + 全部原始统计（报告/审计用，键名稳定）。
#[derive(Debug, Clone, PartialEq)]
pub struct DetectorOutput {
    /// 主分数（阈值判定的对象；语义随检测器：size_min=最短边、white_edge=均值偏差、
    /// sharpness=方差、exposure=死区合计占比）。
    pub score: f64,
    pub pass: bool,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error("未知检测器: {id}（已知: {known}）")]
    UnknownDetector { id: String, known: String },
    #[error("{detector}: 参数错误: {msg}")]
    BadParams { detector: &'static str, msg: String },
    #[error("{detector}: 输入不符: 需要 {expected}，实际 {got}")]
    InputMismatch {
        detector: &'static str,
        expected: &'static str,
        got: &'static str,
    },
    #[error("{detector}: {msg}")]
    Failed { detector: &'static str, msg: String },
}

/// 单条检测器注册项。
pub struct DetectorEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub input: InputKind,
    /// (input, params) → 输出
    pub run: fn(&DetectorInput, &serde_yaml::Value) -> Result<DetectorOutput, DetectorError>,
}

/// 静态注册表（编译进二进制）。
pub static DETECTORS: &[DetectorEntry] = &[
    DetectorEntry {
        id: "size_min",
        name: "尺寸达标",
        input: InputKind::Metadata,
        run: run_size_min,
    },
    DetectorEntry {
        id: "white_edge",
        name: "白底纯净度",
        input: InputKind::Rgb,
        run: run_white_edge,
    },
    DetectorEntry {
        id: "sharpness",
        name: "清晰度（拉普拉斯方差）",
        input: InputKind::Rgb,
        run: run_sharpness,
    },
    DetectorEntry {
        id: "exposure",
        name: "曝光（直方图统计）",
        input: InputKind::Rgb,
        run: run_exposure,
    },
];

/// 判定余量置信度：|score − 阈值| / 阈值，clamp 到 [0,1] 后映射到 [0.5, 1]。
///
/// 语义：检测器自己的判定**离阈值多远**（数据驱动），不是把原始 score 当置信度。
/// 阈值取该检测器的判定阈值参数（size_min.min / white_edge.max_dev / sharpness.min /
/// exposure.max_dead_ratio）；参数缺失/非法 → None（装载期 lint 已先报参数错）。
pub fn margin_confidence(id: &str, params: &serde_yaml::Value, score: f64) -> Option<f64> {
    let threshold = match id {
        "size_min" => params.get("min")?.as_f64()?,
        "white_edge" => params.get("max_dev")?.as_f64()?,
        "sharpness" => params.get("min")?.as_f64()?,
        "exposure" => params.get("max_dead_ratio")?.as_f64()?,
        _ => return None,
    };
    if !(threshold.is_finite() && threshold > 0.0) || !score.is_finite() {
        return None;
    }
    let margin = ((score - threshold).abs() / threshold).clamp(0.0, 1.0);
    Some(0.5 + 0.5 * margin)
}

/// 该检测器是否支持 margin 置信度（装载期 lint 用）。
pub fn supports_margin(id: &str) -> bool {
    matches!(id, "size_min" | "white_edge" | "sharpness" | "exposure")
}

/// 查表（id → 条目）。
pub fn lookup(id: &str) -> Option<&'static DetectorEntry> {
    DETECTORS.iter().find(|e| e.id == id)
}

/// 已知检测器 id 列表（错误信息/调试用）。
pub fn known_ids() -> String {
    DETECTORS
        .iter()
        .map(|e| e.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 顶层分发：未知 id 显式报 UnknownDetector（不静默跳过）。
pub fn run(
    id: &str,
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> Result<DetectorOutput, DetectorError> {
    let entry = lookup(id).ok_or_else(|| DetectorError::UnknownDetector {
        id: id.to_string(),
        known: known_ids(),
    })?;
    (entry.run)(input, params)
}

// ---------- 各检测器的参数解析 + 输入档适配 ----------

/// 参数校验（装载期 lint 用，C8）：与运行期解析**同一个 struct**
/// （`deny_unknown_fields`）→ 白名单零漂移：结构体加字段，lint 自动放行新键。
///
/// 返回 `Some(消息)` = 参数不合法（未知键 / 类型错 / 缺必填）。未知 id → `None`
/// （未知检测器由 [`lookup`] 负责报）。params 非映射（缺省 / null）按**空映射**校验，
/// 缺必填字段照样报错（`check: {detector: sharpness}` 少 params 是错误配置）。
pub fn params_error(id: &str, params: &serde_yaml::Value) -> Option<String> {
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let err = |e: DetectorError| Some(e.to_string());
    match id {
        "size_min" => parse_params::<detectors::SizeMinParams>("size_min", &normalized)
            .err()
            .and_then(err),
        "white_edge" => parse_params::<detectors::WhiteEdgeParams>("white_edge", &normalized)
            .err()
            .and_then(err),
        "sharpness" => parse_params::<detectors::SharpnessParams>("sharpness", &normalized)
            .err()
            .and_then(err),
        "exposure" => parse_params::<detectors::ExposureParams>("exposure", &normalized)
            .err()
            .and_then(err),
        _ => None,
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(
    detector: &'static str,
    params: &serde_yaml::Value,
) -> Result<T, DetectorError> {
    serde_yaml::from_value(params.clone()).map_err(|e| DetectorError::BadParams {
        detector,
        msg: e.to_string(),
    })
}

fn expect_rgb<'a>(
    detector: &'static str,
    input: &'a DetectorInput,
) -> Result<(u32, u32, &'a [u8]), DetectorError> {
    match input {
        DetectorInput::Rgb {
            width,
            height,
            pixels,
        } => Ok((*width, *height, pixels)),
        other => Err(DetectorError::InputMismatch {
            detector,
            expected: "Rgb",
            got: kind_name(other),
        }),
    }
}

fn expect_metadata(
    detector: &'static str,
    input: &DetectorInput,
) -> Result<(u32, u32), DetectorError> {
    match input {
        DetectorInput::Metadata { width, height } => Ok((*width, *height)),
        other => Err(DetectorError::InputMismatch {
            detector,
            expected: "Metadata",
            got: kind_name(other),
        }),
    }
}

fn kind_name(input: &DetectorInput) -> &'static str {
    match input {
        DetectorInput::Metadata { .. } => "Metadata",
        DetectorInput::Rgb { .. } => "Rgb",
    }
}

fn run_size_min(
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> Result<DetectorOutput, DetectorError> {
    let p: detectors::SizeMinParams = parse_params("size_min", params)?;
    let (width, height) = expect_metadata("size_min", input)?;
    let r = detectors::size_min(width, height, &p);
    let mut metrics = BTreeMap::new();
    metrics.insert("min_side".into(), r.min_side as f64);
    metrics.insert("required".into(), r.required as f64);
    Ok(DetectorOutput {
        score: r.min_side as f64,
        pass: r.pass,
        metrics,
    })
}

fn run_white_edge(
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> Result<DetectorOutput, DetectorError> {
    let p: detectors::WhiteEdgeParams = parse_params("white_edge", params)?;
    let (width, height, pixels) = expect_rgb("white_edge", input)?;
    let r =
        detectors::white_edge(pixels, width, height, &p).map_err(|msg| DetectorError::Failed {
            detector: "white_edge",
            msg,
        })?;
    let mut metrics = BTreeMap::new();
    metrics.insert("mean_dev".into(), r.mean_dev);
    metrics.insert("max_pixel_dev".into(), r.max_dev);
    metrics.insert("band".into(), r.band as f64);
    metrics.insert("edge_pixels".into(), r.edge_pixels as f64);
    metrics.insert("threshold".into(), p.max_dev);
    Ok(DetectorOutput {
        score: r.mean_dev,
        pass: r.pass,
        metrics,
    })
}

fn run_sharpness(
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> Result<DetectorOutput, DetectorError> {
    let p: detectors::SharpnessParams = parse_params("sharpness", params)?;
    let (width, height, pixels) = expect_rgb("sharpness", input)?;
    let luma = detectors::luma_from_rgb(pixels);
    let (variance, n) =
        detectors::sharpness_laplacian_variance(&luma, width, height).map_err(|msg| {
            DetectorError::Failed {
                detector: "sharpness",
                msg,
            }
        })?;
    let mut metrics = BTreeMap::new();
    metrics.insert("variance".into(), variance);
    metrics.insert("required_min".into(), p.min);
    metrics.insert("pixels".into(), n as f64);
    Ok(DetectorOutput {
        score: variance,
        pass: variance >= p.min,
        metrics,
    })
}

fn run_exposure(
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> Result<DetectorOutput, DetectorError> {
    let p: detectors::ExposureParams = parse_params("exposure", params)?;
    let (width, height, pixels) = expect_rgb("exposure", input)?;
    let luma = detectors::luma_from_rgb(pixels);
    let r = detectors::exposure(&luma, width, height, &p).map_err(|msg| DetectorError::Failed {
        detector: "exposure",
        msg,
    })?;
    let mut metrics = BTreeMap::new();
    metrics.insert("dead_ratio".into(), r.dead_ratio);
    metrics.insert("dead_black_ratio".into(), r.dead_black_ratio);
    metrics.insert("dead_white_ratio".into(), r.dead_white_ratio);
    metrics.insert("mean".into(), r.mean);
    metrics.insert("threshold".into(), p.max_dead_ratio);
    metrics.insert("min_mean".into(), p.min_mean);
    metrics.insert("max_mean".into(), p.max_mean);
    Ok(DetectorOutput {
        score: r.dead_ratio,
        pass: r.pass,
        metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn dispatch_size_min_via_registry() {
        let out = run(
            "size_min",
            &DetectorInput::Metadata {
                width: 700,
                height: 700,
            },
            &yaml("{min: 700}"),
        )
        .unwrap();
        assert!(out.pass);
        assert_eq!(out.score, 700.0);
        assert_eq!(out.metrics["min_side"], 700.0);
        // 压线边界：min == min_side 合格，+1 不合格
        let out = run(
            "size_min",
            &DetectorInput::Metadata {
                width: 330,
                height: 247,
            },
            &yaml("{min: 247}"),
        )
        .unwrap();
        assert!(out.pass);
        let out = run(
            "size_min",
            &DetectorInput::Metadata {
                width: 330,
                height: 247,
            },
            &yaml("{min: 248}"),
        )
        .unwrap();
        assert!(!out.pass);
    }

    #[test]
    fn unknown_detector_fails_loudly() {
        let err = run(
            "fg_ratio",
            &DetectorInput::Metadata {
                width: 10,
                height: 10,
            },
            &yaml("{}"),
        )
        .unwrap_err();
        assert!(matches!(err, DetectorError::UnknownDetector { .. }));
        let msg = err.to_string();
        assert!(msg.contains("未知检测器"), "msg = {msg}");
        assert!(msg.contains("fg_ratio"), "msg = {msg}");
        assert!(msg.contains("size_min"), "msg = {msg}");
    }

    #[test]
    fn input_mismatch_and_bad_params_fail_loudly() {
        // size_min 要元数据档，给像素档 → 显式报错
        let err = run(
            "size_min",
            &DetectorInput::Rgb {
                width: 10,
                height: 10,
                pixels: vec![255; 300],
            },
            &yaml("{min: 700}"),
        )
        .unwrap_err();
        assert!(matches!(err, DetectorError::InputMismatch { .. }));
        // white_edge 缺 max_dev → 参数错误（缺必填当场报）
        let err = run(
            "white_edge",
            &DetectorInput::Rgb {
                width: 10,
                height: 10,
                pixels: vec![255; 300],
            },
            &yaml("{band: 4}"),
        )
        .unwrap_err();
        assert!(matches!(err, DetectorError::BadParams { .. }));
        // 拼错参数键 → 参数错误（deny_unknown_fields，不允许静默忽略）
        let err = run(
            "exposure",
            &DetectorInput::Rgb {
                width: 10,
                height: 10,
                pixels: vec![255; 300],
            },
            &yaml("{max_dead_ratio: 0.02, white_cut_line: 10}"),
        )
        .unwrap_err();
        assert!(matches!(err, DetectorError::BadParams { .. }));
    }

    #[test]
    fn pixel_detectors_end_to_end_via_registry() {
        // 10x10 白底 + 中心黑块：white_edge 合格 / sharpness 有值 / exposure 死区为 0
        let mut rgb = vec![255u8; 10 * 10 * 3];
        for y in 4..6 {
            for x in 4..6 {
                let i = (y * 10 + x) * 3;
                rgb[i..i + 3].fill(0);
            }
        }
        let input = DetectorInput::Rgb {
            width: 10,
            height: 10,
            pixels: rgb,
        };
        let we = run("white_edge", &input, &yaml("{max_dev: 8.0}")).unwrap();
        assert!(we.pass);
        assert_eq!(we.score, 0.0);
        let sh = run("sharpness", &input, &yaml("{min: 100.0}")).unwrap();
        assert!(sh.pass, "黑心白边对比强，方差 >= 100");
        assert!(sh.score > 100.0);
        let ex = run("exposure", &input, &yaml("{max_dead_ratio: 0.02}")).unwrap();
        // 白底 96/100 达死白线（≥245）+ 黑块 4/100 达死黑线（≤10）→ 死区 100%
        // （白底图过 white_edge 不过 exposure 是预期语义：两规则各管各的维度）
        assert!((ex.score - 1.0).abs() < 1e-12);
        assert!(!ex.pass);
        assert_eq!(ex.metrics["dead_black_ratio"], 0.04);
        assert_eq!(ex.metrics["dead_white_ratio"], 0.96);
    }

    #[test]
    fn lookup_and_list() {
        assert_eq!(DETECTORS.len(), 4);
        assert_eq!(lookup("sharpness").unwrap().input, InputKind::Rgb);
        assert_eq!(lookup("size_min").unwrap().input, InputKind::Metadata);
        assert!(
            lookup("vl_judge").is_none(),
            "vl/模型检测器不在 classic 注册表"
        );
        assert!(known_ids().contains("exposure"));
    }
}
