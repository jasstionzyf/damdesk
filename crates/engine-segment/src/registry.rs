//! local_model 检测器静态注册表（C3；与 engine-classic::registry 同纪律：进程内静态
//! 注册、未知 id 显式报错、输入档/参数错误响亮失败。类型独立——engine-segment 与
//! engine-classic 同为 L1，禁止互引；svc-pipeline（L2）持有两套注册表的分发）。
//!
//! 加检测器 = 这里加函数 + DETECTORS 加一条。C3 仅 `fg_ratio`（u2net 前景占比，
//! 电商包 R4_subject_ratio 的判定器）。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{segmenter, Result as SegResult, SegmentError, U2NET};

/// 检测器输入（像素档；分割必须解码，无元数据档）。
#[derive(Debug, Clone)]
pub struct SegmentInput {
    pub width: u32,
    pub height: u32,
    /// 交错 RGB，len = w*h*3
    pub pixels: Vec<u8>,
}

/// 检测输出（与 engine_classic::registry::DetectorOutput 同构：score/pass/metrics）。
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentOutput {
    /// 主分数 = fg_ratio
    pub score: f64,
    pub pass: bool,
    pub metrics: BTreeMap<String, f64>,
}

pub struct DetectorEntry {
    pub id: &'static str,
    pub name: &'static str,
    /// (input, params) → 输出
    pub run: fn(&SegmentInput, &serde_yaml::Value) -> Result<SegmentOutput, SegmentError>,
}

/// fg_ratio 参数（check.params）：`{model: u2net, min: 0.65, max: 0.85}`；
/// model 可省略（默认 u2net），min/max 必填（缺 = 参数错误，响亮）。
/// C8：补 `deny_unknown_fields` —— 与 classic 检测器同纪律（写错的参数键必须当场报错，
/// 不许静默忽略；装载期 lint 也复用本 struct，见 `params_error`）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FgRatioParams {
    #[serde(default)]
    model: Option<String>,
    min: f64,
    max: f64,
}

/// 检测器所需模型名（编排器在解码前做 model_ready 预检 → skipped(model_not_downloaded)）。
pub fn required_model(detector: &str, params: &serde_yaml::Value) -> SegResult<String> {
    match detector {
        "fg_ratio" => {
            let p: FgRatioParams = serde_yaml::from_value(params.clone()).map_err(|e| {
                SegmentError::BadParams {
                    detector: "fg_ratio",
                    msg: e.to_string(),
                }
            })?;
            Ok(p.model.unwrap_or_else(|| U2NET.to_string()))
        }
        other => Err(SegmentError::InputMismatch {
            detector: "registry",
            msg: format!("未知检测器: {other}（已知: {}）", known_ids()),
        }),
    }
}

/// 参数校验（装载期 lint 用，C8）：与运行期同一 struct（`deny_unknown_fields`）→ 白名单零漂移。
/// 未知 id → `None`（未知检测器由 `lookup` 负责）；params 非映射按空映射校验。
pub fn params_error(id: &str, params: &serde_yaml::Value) -> Option<String> {
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    match id {
        "fg_ratio" => required_model(id, &normalized).err().map(|e| e.to_string()),
        _ => None,
    }
}

fn run_fg_ratio(input: &SegmentInput, params: &serde_yaml::Value) -> SegResult<SegmentOutput> {
    let p: FgRatioParams = serde_yaml::from_value(params.clone()).map_err(|e| {
        SegmentError::BadParams {
            detector: "fg_ratio",
            msg: e.to_string(),
        }
    })?;
    if !(p.min >= 0.0 && p.max <= 1.0 && p.min < p.max) {
        return Err(SegmentError::BadParams {
            detector: "fg_ratio",
            msg: format!("要求 0 ≤ min < max ≤ 1，实际 min={} max={}", p.min, p.max),
        });
    }
    let model = p.model.unwrap_or_else(|| U2NET.to_string());
    let seg = segmenter(&model)?;
    let s = seg.run_rgb(input.width, input.height, &input.pixels)?;
    let pass = s.fg_ratio >= p.min && s.fg_ratio <= p.max;
    let mut metrics = BTreeMap::new();
    metrics.insert("fg_ratio".into(), s.fg_ratio);
    metrics.insert("side".into(), crate::SIDE as f64);
    metrics.insert("threshold".into(), crate::FG_THRESHOLD as f64);
    if let Some([x0, y0, x1, y1]) = s.bbox {
        metrics.insert("bbox_x0".into(), x0 as f64);
        metrics.insert("bbox_y0".into(), y0 as f64);
        metrics.insert("bbox_x1".into(), x1 as f64);
        metrics.insert("bbox_y1".into(), y1 as f64);
    }
    Ok(SegmentOutput {
        score: s.fg_ratio,
        pass,
        metrics,
    })
}

/// 静态注册表（编译进二进制）。
pub static DETECTORS: &[DetectorEntry] = &[DetectorEntry {
    id: "fg_ratio",
    name: "主体前景占比（u2net）",
    run: run_fg_ratio,
}];

/// 查表（id → 条目）。
pub fn lookup(id: &str) -> Option<&'static DetectorEntry> {
    DETECTORS.iter().find(|e| e.id == id)
}

/// 已知检测器 id 列表（错误信息用）。
pub fn known_ids() -> String {
    DETECTORS.iter().map(|e| e.id).collect::<Vec<_>>().join(", ")
}

/// 顶层分发：未知 id 显式报错（不静默跳过）。
pub fn run(
    id: &str,
    input: &SegmentInput,
    params: &serde_yaml::Value,
) -> SegResult<SegmentOutput> {
    let entry = lookup(id).ok_or_else(|| SegmentError::InputMismatch {
        detector: "registry",
        msg: format!("未知检测器: {id}（已知: {}）", known_ids()),
    })?;
    (entry.run)(input, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(w: u32, h: u32) -> SegmentInput {
        SegmentInput {
            width: w,
            height: h,
            pixels: vec![128; (w * h * 3) as usize],
        }
    }

    #[test]
    fn unknown_detector_fails_loudly() {
        let err = run("white_edge", &input(4, 4), &serde_yaml::Value::Null).unwrap_err();
        assert!(err.to_string().contains("未知检测器"));
        assert!(err.to_string().contains("fg_ratio"));
        assert!(lookup("fg_ratio").is_some());
        assert!(lookup("nope").is_none());
        assert_eq!(required_model("nope", &serde_yaml::Value::Null).is_err(), true);
    }

    #[test]
    fn fg_ratio_params_validation() {
        // 读侧用例（最后一段断言「模型根未配置 → 响亮报错」）：必须与同二进制里改根/改
        // `DAM_MODELS_ROOT` 的用例互斥，否则会被它们的临时根带偏（卡 t_39b4891a 现象：
        // 上面那个用例把根指到含 stub u2net.onnx 的临时目录 → 本用例拿到 Onnx 错）。
        let _scope = crate::ModelsRootScope::enter();
        // min/max 必填
        let err = required_model("fg_ratio", &serde_yaml::from_str("{}").unwrap()).unwrap_err();
        assert!(err.to_string().contains("参数错误"));
        // model 可省略 → u2net 默认
        let m = required_model("fg_ratio", &serde_yaml::from_str("{min: 0.1, max: 0.9}").unwrap())
            .unwrap();
        assert_eq!(m, U2NET);
        let m = required_model(
            "fg_ratio",
            &serde_yaml::from_str("{model: u2net, min: 0.1, max: 0.9}").unwrap(),
        )
        .unwrap();
        assert_eq!(m, U2NET);
        // 区间非法（0 ≤ min < max ≤ 1）：run 在查模型之前先响亮拒绝
        let err = run(
            "fg_ratio",
            &input(4, 4),
            &serde_yaml::from_str("{min: 0.9, max: 0.1}").unwrap(),
        )
        .unwrap_err();
        assert!(matches!(err, SegmentError::BadParams { .. }), "got {err:?}");
        // run 在模型未配置根时报错响亮（不 panic、不静默 pass）
        let err = run(
            "fg_ratio",
            &input(4, 4),
            &serde_yaml::from_str("{min: 0.1, max: 0.9}").unwrap(),
        )
        .unwrap_err();
        assert!(
            matches!(err, SegmentError::NoModelsRoot | SegmentError::ModelMissing(_)),
            "got {err:?}"
        );
    }
}
