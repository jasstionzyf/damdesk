//! 超分包参数 schema × 宿主参数解析器（N2 机制 / 卡 task）。
//!
//! 验的是**装机期会不会红**：schema 能不能被宿主解析、默认层对不对、`when` 有没有悬空引用、
//! 越界值会不会被拒。以前这类问题要等用户装上包、打开 ParamForm 才暴露。
use std::path::{Path, PathBuf};

use pack_host::params::ParamSchema;
use serde_json::json;

fn pack_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .join("packs/plugins/upscale")
}

fn schema() -> ParamSchema {
    ParamSchema::load_declared(&pack_dir(), Some("resources/params.schema.json"))
        .expect("超分包 schema 必须能被宿主解析")
        .expect("manifest 声明了 params")
}

#[test]
fn schema_parses_and_declares_expected_params() {
    let s = schema();
    let keys: Vec<&str> = s.properties().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["denoise", "engine", "scale", "tile"],
        "参数集合变了要同步卡面口径"
    );
    assert_eq!(s.required(), ["scale"]);
}

#[test]
fn defaults_are_scale2_full_auto_no_denoise() {
    let s = schema();
    let effective = s
        .resolve_json(&[("plugin", None)])
        .expect("默认层必须可解析");
    assert_eq!(
        effective["scale"],
        json!("2"),
        "scale 是枚举（宿主必须在装包期就拒掉 3）"
    );
    assert_eq!(effective["engine"], json!("full"));
    assert_eq!(effective["tile"], json!("auto"));
    assert_eq!(effective["denoise"], json!(0));
}

#[test]
fn spec_layer_overrides_and_is_visible() {
    let s = schema();
    let spec = json!({"scale": "4", "engine": "light"});
    let effective = s
        .resolve_json(&[("plugin", None), ("spec", Some(&spec))])
        .expect("规格层覆盖必须成立");
    assert_eq!(effective["scale"], json!("4"));
    assert_eq!(effective["engine"], json!("light"));
    assert_eq!(effective["tile"], json!("auto"), "未覆盖的键保留默认值");
}

/// 宿主旧口径的坑：`integer + min/max/step=2` **不校验「是否为 step 整数倍」**，
/// scale=3 会一路过校验跑到插件里才被拒。所以这里用 enum 声明，让装包期就拒掉。
#[test]
fn scale_outside_2_or_4_rejected_by_schema() {
    let s = schema();
    for bad in [json!("3"), json!("1"), json!("8"), json!(3)] {
        let spec = json!({"scale": bad});
        let err = s
            .resolve_json(&[("plugin", None), ("spec", Some(&spec))])
            .expect_err("scale 只允许 2/4（枚举）");
        let msg = format!("{err}");
        assert!(msg.contains("scale"), "错误要点名参数：{msg}");
    }
}

#[test]
fn engine_outside_enum_rejected() {
    let s = schema();
    let spec = json!({"engine": "gpu"});
    let err = s
        .resolve_json(&[("plugin", None), ("spec", Some(&spec))])
        .expect_err("engine 是枚举，未知值必须拒");
    assert!(format!("{err}").contains("engine"), "{err}");
}

#[test]
fn tile_outside_enum_rejected() {
    let s = schema();
    let spec = json!({"tile": "9999"});
    let err = s
        .resolve_json(&[("plugin", None), ("spec", Some(&spec))])
        .expect_err("tile 是枚举，未知值必须拒");
    assert!(format!("{err}").contains("tile"), "{err}");
}

#[test]
fn denoise_range_enforced() {
    let s = schema();
    for bad in [json!(-0.1), json!(1.2)] {
        let spec = json!({"denoise": bad});
        let err = s
            .resolve_json(&[("plugin", None), ("spec", Some(&spec))])
            .expect_err("denoise 必须在 0..1");
        assert!(format!("{err}").contains("denoise"), "{err}");
    }
}

#[test]
fn unknown_key_rejected_with_layer_name() {
    let s = schema();
    let spec = json!({"sharpen": 0.5});
    let err = s
        .resolve_json(&[("spec", Some(&spec))])
        .expect_err("未知键必须拒（静默忽略 = 用户以为生效了）");
    assert!(format!("{err}").contains("sharpen"), "{err}");
}

#[test]
fn scale_is_marked_affects_body() {
    let s = schema();
    let danger = s.danger_keys();
    assert!(
        danger.contains(&"scale"),
        "超分改的是图片本体（只在工作版本上改）：{danger:?}"
    );
}
