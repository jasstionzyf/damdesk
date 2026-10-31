//! N6a 几何组参数片段 × 宿主参数解析器（`pack_host::params`）的联调钉。
//!
//! 为什么要这一条：`packs/plugins/imaging/resources/params.geometry.json` 是几何组的 schema 片段，
//! N6d 会把它并进 imaging 插件的 `resources/params.schema.json`。片段只要有一个键/类型不合法，
//! 宿主就是**整包拒装**（`pack_params_schema`），而那时错误暴露在装机期、离作者最远。
//! 这里用宿主**同一份**解析器（`ParamSchema::parse` + `resolve_json`）在 L0 就把关：
//!   1. 片段能解析（未知键/类型错/枚举缺 options/`when` 指向未声明参数 → 直接红）；
//!   2. 默认层能合并出**全量生效值**，且 `when` 不成立的键被剔掉（`prune_invisible` 行为）；
//!   3. 越界值被拒（`check` 真在拦，不是摆设）。
//!
//! 片段改动后先跑 `python3 packs/plugins/imaging/tests/dump_params_schema.py` 再跑本测试。

use std::path::{Path, PathBuf};

use pack_host::params::ParamSchema;
use serde_json::{json, Map, Value};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn fragment_path() -> PathBuf {
    repo_root().join("packs/plugins/imaging/resources/params.geometry.json")
}

fn load() -> ParamSchema {
    let path = fragment_path();
    let raw = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("读不到几何组 schema 片段 {}：{e}", path.display()));
    ParamSchema::parse(&raw).unwrap_or_else(|e| panic!("宿主参数解析器拒绝了片段：{e}"))
}

fn map(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in pairs {
        out.insert((*k).to_string(), v.clone());
    }
    out
}

#[test]
fn fragment_parses_with_host_parser() {
    let schema = load();
    assert_eq!(schema.schema_version(), 1);
    let props = schema.properties();
    // 几何组必须声明全部缩放方式参数 + crop 的四个决策参数
    for key in [
        "resizeMode",
        "resizeLongEdge",
        "resizeShortEdge",
        "resizeWidth",
        "resizeHeight",
        "resizeScale",
        "resizeAllowUpscale",
        "cropAspect",
        "cropAspectCustom",
        "cropAnchor",
        "cropPadding",
        "cropBackground",
    ] {
        assert!(props.contains_key(key), "片段缺参数 {key}");
    }
    assert_eq!(schema.groups().len(), 1);
    assert_eq!(schema.groups()[0].key, "geometry");
    assert!(schema.required().is_empty(), "几何组参数全有默认值，不应 required");
}

#[test]
fn defaults_resolve_and_prune_invisible() {
    let schema = load();
    // 默认层（不传任何规格/任务层）：长边模式生效，短边/固定宽高/系数被 when 剔掉
    let effective = schema
        .resolve_json(&[("plugin-default", None)])
        .expect("默认层合并失败");
    assert_eq!(effective.get("resizeMode"), Some(&json!("long_edge")));
    assert_eq!(effective.get("resizeLongEdge"), Some(&json!(1600)));
    assert_eq!(effective.get("cropAspect"), Some(&json!("1:1")));
    assert_eq!(effective.get("cropAnchor"), Some(&json!("center")));
    assert_eq!(effective.get("cropPadding"), Some(&json!(0.05)));
    assert_eq!(effective.get("resizeAllowUpscale"), Some(&json!(false)));
    for pruned in ["resizeShortEdge", "resizeWidth", "resizeHeight", "resizeScale",
                   "cropAspectCustom"] {
        assert!(!effective.contains_key(pruned), "{pruned} 应在 when 不成立时被剔掉");
    }

    // 规格层切到 short_edge：反过来剔掉 long_edge，短边参数生效
    let spec = map(&[("resizeMode", json!("short_edge")), ("resizeShortEdge", json!(1200))]);
    let effective = schema
        .resolve_json(&[("plugin-default", None), ("spec", Some(&Value::Object(spec)))])
        .expect("规格层合并失败");
    assert_eq!(effective.get("resizeShortEdge"), Some(&json!(1200)));
    assert!(!effective.contains_key("resizeLongEdge"), "切模式后长边参数应被剔掉");

    // custom 比例：自定义串才可见
    let spec = map(&[("cropAspect", json!("custom")), ("cropAspectCustom", json!("3:5"))]);
    let effective = schema
        .resolve_json(&[("spec", Some(&Value::Object(spec)))])
        .expect("custom 比例合并失败");
    assert_eq!(effective.get("cropAspectCustom"), Some(&json!("3:5")));
}

#[test]
fn out_of_range_and_unknown_keys_are_rejected() {
    let schema = load();
    // 留白越界（schema max=0.2）
    let bad = map(&[("cropPadding", json!(0.5))]);
    assert!(schema
        .resolve_json(&[("spec", Some(&Value::Object(bad)))])
        .is_err(), "cropPadding=0.5 越界必须被拒");
    // 未知键（宿主整包拒装的那一类）
    let bad = map(&[("cropPaddingg", json!(0.1))]);
    assert!(schema
        .resolve_json(&[("spec", Some(&Value::Object(bad)))])
        .is_err(), "未知参数键必须被拒");
    // 枚举越界
    let bad = map(&[("resizeMode", json!("longest_edge"))]);
    assert!(schema
        .resolve_json(&[("spec", Some(&Value::Object(bad)))])
        .is_err(), "resizeMode 不在 options 内必须被拒");
    // 比例把 0.05..0.2 的边界值放行
    let good = map(&[("cropPadding", json!(0.2))]);
    assert!(schema
        .resolve_json(&[("spec", Some(&Value::Object(good)))])
        .is_ok(), "cropPadding=0.2 是合法上界");
}
