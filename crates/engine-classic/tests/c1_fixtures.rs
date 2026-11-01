//! C1 验收（fixtures 真图段）：tests/fixtures/dog.jpg + car.jpg 逐个过注册表分发，
//! 四检测器全跑一遍 + size_min 阈值边界用例。像素检测器输入 = image crate 解码的
//! RGB 缓冲（dev-dep 仅测试用，engine-classic 本体仍无解码依赖）。

use std::collections::BTreeMap;

use engine_classic::registry::{run, DetectorInput};

fn fixture(name: &str) -> (u32, u32, Vec<u8>) {
    let path = format!(
        "{}/../../tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let img = image::ImageReader::open(&path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
        .with_guessed_format()
        .unwrap_or_else(|e| panic!("format {path}: {e}"))
        .decode()
        .unwrap_or_else(|e| panic!("decode {path}: {e}"));
    let (w, h) = (img.width(), img.height());
    (w, h, img.to_rgb8().into_raw())
}

fn yaml(s: &str) -> serde_yaml::Value {
    serde_yaml::from_str(s).unwrap()
}

fn assert_deterministic(id: &str, input: &DetectorInput, params: &serde_yaml::Value) {
    let a = run(id, input, params).unwrap();
    let b = run(id, input, params).unwrap();
    assert_eq!(a, b, "{id} 两次执行结果不一致（复现性铁律）");
}

#[test]
fn size_min_real_fixtures_with_boundary() {
    // dog.jpg 330x231、car.jpg 330x247（见 tests/fixtures/README.md）
    for (name, expected_min_side) in [("dog.jpg", 231u32), ("car.jpg", 247)] {
        let (w, h, _) = fixture(name);
        assert_eq!(
            w.min(h),
            expected_min_side,
            "{name} 尺寸与 fixture 契约不符"
        );
        // 阈值边界：min == 最短边 → 合格；+1 → 不合格
        let input = DetectorInput::Metadata {
            width: w,
            height: h,
        };
        let eq = run(
            "size_min",
            &input,
            &yaml(&format!("{{min: {expected_min_side}}}")),
        )
        .unwrap();
        assert!(eq.pass, "{name} min={expected_min_side} 应压线合格");
        assert_eq!(eq.score, expected_min_side as f64);
        let gt = run(
            "size_min",
            &input,
            &yaml(&format!("{{min: {}}}", expected_min_side + 1)),
        )
        .unwrap();
        assert!(!gt.pass, "{name} min={} 应不合格", expected_min_side + 1);
        // 淘宝主图 700 下限：两张都远不达标
        let taobao = run("size_min", &input, &yaml("{min: 700}")).unwrap();
        assert!(!taobao.pass);
    }
}

#[test]
fn white_edge_real_fixtures_are_not_white_bg() {
    // 真照片（草地/车道）不是纯白底 → 均值偏差应显著 > 淘宝阈值 8，判定不合格；
    // 且同一输入两次执行结果完全一致（可复现）。
    for name in ["dog.jpg", "car.jpg"] {
        let (w, h, px) = fixture(name);
        let input = DetectorInput::Rgb {
            width: w,
            height: h,
            pixels: px,
        };
        assert_deterministic("white_edge", &input, &yaml("{max_dev: 8.0}"));
        let out = run("white_edge", &input, &yaml("{max_dev: 8.0}")).unwrap();
        assert!(out.score > 8.0, "{name} mean_dev={} 应 > 8", out.score);
        assert!(!out.pass);
        assert!(out.score < 255.0);
        // band=4，短边 > 2*band → 边缘带 = 全幅 - 内圈（w-2band)x(h-2band)
        assert_eq!(out.metrics["edge_pixels"] as u64, {
            (w as u64) * (h as u64) - (w as u64 - 8) * (h as u64 - 8)
        });
    }
}

#[test]
fn sharpness_real_fixtures_have_texture() {
    // 真照片有纹理 → 拉普拉斯方差 > 0 且大于平坦阈值；输出含全部审计指标。
    for name in ["dog.jpg", "car.jpg"] {
        let (w, h, px) = fixture(name);
        let input = DetectorInput::Rgb {
            width: w,
            height: h,
            pixels: px,
        };
        assert_deterministic("sharpness", &input, &yaml("{min: 1.0}"));
        let out = run("sharpness", &input, &yaml("{min: 1.0}")).unwrap();
        assert!(out.score > 1.0, "{name} variance={} 应 > 1", out.score);
        assert_eq!(
            out.metrics["pixels"] as u64,
            (w as u64 - 2) * (h as u64 - 2)
        );
    }
}

#[test]
fn exposure_real_fixtures_well_exposed_and_reported() {
    // 真照片既不该死黑一片也不该死白一片 → 死区合计占比 <= 0.5、均值亮度在 0-255。
    for name in ["dog.jpg", "car.jpg"] {
        let (w, h, px) = fixture(name);
        let input = DetectorInput::Rgb {
            width: w,
            height: h,
            pixels: px,
        };
        assert_deterministic("exposure", &input, &yaml("{max_dead_ratio: 0.5}"));
        let out = run("exposure", &input, &yaml("{max_dead_ratio: 0.5}")).unwrap();
        assert!(out.score <= 0.5, "{name} dead_ratio={}", out.score);
        assert!(out.metrics["mean"] > 0.0 && out.metrics["mean"] < 255.0);
        assert_eq!(
            out.metrics["dead_black_ratio"] + out.metrics["dead_white_ratio"],
            out.score
        );
    }
}

#[test]
fn output_metrics_are_stable_keyed() {
    // 报告审计契约：四检测器的 metrics 键名集合稳定（C2 报告列依赖）
    let (w, h, px) = fixture("dog.jpg");
    let rgb_in = DetectorInput::Rgb {
        width: w,
        height: h,
        pixels: px,
    };
    let meta_in = DetectorInput::Metadata {
        width: w,
        height: h,
    };
    let expected: &[(&str, &[&str])] = &[
        ("size_min", &["min_side", "required"]),
        (
            "white_edge",
            &[
                "mean_dev",
                "max_pixel_dev",
                "band",
                "edge_pixels",
                "threshold",
            ],
        ),
        ("sharpness", &["variance", "required_min", "pixels"]),
        (
            "exposure",
            &[
                "dead_ratio",
                "dead_black_ratio",
                "dead_white_ratio",
                "mean",
                "threshold",
                "min_mean",
                "max_mean",
            ],
        ),
    ];
    let params_by_id = |id: &str| match id {
        "size_min" => "{min: 100}",
        "white_edge" => "{max_dev: 8.0}",
        "sharpness" => "{min: 1.0}",
        _ => "{max_dead_ratio: 0.5}",
    };
    for (id, keys) in expected {
        let input = if *id == "size_min" { &meta_in } else { &rgb_in };
        let out = run(id, input, &yaml(params_by_id(id))).unwrap();
        let got: Vec<&String> = out.metrics.keys().collect();
        let mut want: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        want.sort(); // BTreeMap 迭代序 = 字典序
        assert_eq!(
            got,
            want.iter().collect::<Vec<_>>(),
            "{id} metrics 键集漂移"
        );
        let _: &BTreeMap<String, f64> = &out.metrics;
    }
}
