//! 发布形态 × 官方包能力面矩阵（t_c3e0417a 发布口径裁决的机器可读锁）。
//!
//! 能力面（`default_host_capabilities()`）是**构建产物**属性：`local-infer` 编入与否
//! 决定官方包谁能装。本文件把裁决结论钉成断言，两种形态各跑一次（publish_gate step 9）：
//!
//! ```text
//! cargo test -p pack-host --test form_capabilities                        # 带 local-infer（桌面 + web 正式发布）
//! cargo test -p pack-host --no-default-features --test form_capabilities  # 裸 web 形态（无本地推理）
//! ```
//!
//! 期望矩阵：
//! - 带 local-infer：5/5 官方包能力齐备（9 项能力全放行）；
//! - 不带：realestate / upscale 可装；imaging / segment 缺 `detect.u2net`；
//!   ecommerce 缺 `detect.ocr` + `detect.u2net` + `op.generative`。
//!
//! 门禁意义：官方包以后加了新能力、或 `default_host_capabilities()` 漂移、
//! 或发版构建漏了 `--features local-infer` → 这里立刻红，而不是等 self-host 用户装包时
//! 才发现「官方包装不进」。
//!
//! 包清单**真读** `packs/plugins/*/manifest.json`，不造合成夹具：合成数据自证等于没测。

use std::path::{Path, PathBuf};

use pack_host::{
    default_host_capabilities, load_pack, missing_capabilities, Manifest, RegistryError,
    KNOWN_CAPABILITIES,
};

/// 官方包根目录（本 crate 位于 `<repo>/crates/pack-host`）。
fn official_pack_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/plugins")
}

/// 读全部官方包 manifest（排序后返回，断言输出稳定可 diff）。
fn official_packs() -> Vec<Manifest> {
    let root = official_pack_root();
    let entries = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("读不到官方包目录 {root:?}：{e}（发布形态矩阵必须对真清单成立）"));
    let mut packs: Vec<(String, Manifest)> = Vec::new();
    for entry in entries {
        let dir = entry.expect("官方包目录项读取失败").path();
        if !dir.is_dir() {
            continue;
        }
        let (manifest, _identity) =
            load_pack(&dir).unwrap_or_else(|e| panic!("{dir:?} 的 manifest 不合法：{e}"));
        packs.push((manifest.id.clone(), manifest));
    }
    packs.sort_by(|a, b| a.0.cmp(&b.0));
    packs.into_iter().map(|(_, m)| m).collect()
}

/// 官方包 id → 本宿主缺失能力（已排序）。
fn missing_by_id() -> Vec<(String, Vec<String>)> {
    let host = default_host_capabilities();
    let mut rows: Vec<(String, Vec<String>)> = official_packs()
        .into_iter()
        .map(|m| {
            let miss = missing_capabilities(&m, &host);
            (m.id.clone(), miss)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// 官方包 id → 缺失能力（只有裸形态断言用，带 local-infer 形态断言全部为空、直接遍历）。
#[cfg(not(feature = "local-infer"))]
fn missing_of(rows: &[(String, Vec<String>)], id: &str) -> Vec<String> {
    rows.iter()
        .find(|(pid, _)| pid == id)
        .unwrap_or_else(|| panic!("官方包里没有 {id}（当前：{:?}）", rows.iter().map(|r| &r.0).collect::<Vec<_>>()))
        .1
        .clone()
}

/// 两形态共用：宿主宣称的能力必须是协议已知能力（不许编造能力名绕过门）。
#[test]
fn host_capabilities_are_all_protocol_known() {
    let host = default_host_capabilities();
    for cap in &host {
        assert!(
            KNOWN_CAPABILITIES.contains(cap),
            "宿主宣称了协议外的能力 {cap}（已知：{KNOWN_CAPABILITIES:?}）"
        );
    }
    assert_eq!(host.len(), host.iter().collect::<std::collections::BTreeSet<_>>().len(), "能力表不许有重复");
}

/// 两形态共用：官方包集合与 manifest 能力声明都合法（防止「装了但谁也不认」的漂移）。
#[test]
fn official_packs_are_five_with_known_capabilities() {
    let packs = official_packs();
    let ids: Vec<&str> = packs.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "com.soujpg.pack.ecommerce",
            "com.soujpg.pack.imaging",
            "com.soujpg.pack.realestate",
            "com.soujpg.pack.segment",
            "com.soujpg.pack.upscale",
        ],
        "官方包集合变了（增删官方包属发布面变更，须同步本矩阵与 docs）"
    );
    for p in &packs {
        assert!(!p.capabilities.is_empty(), "{} 没声明任何能力", p.id);
        for c in &p.capabilities {
            assert!(KNOWN_CAPABILITIES.contains(&c.as_str()), "{} 声明了未知能力 {c}", p.id);
        }
    }
}

/// 带 `local-infer` 的构建（桌面版 + web/容器正式发布）→ 官方 5 包全部能力齐备。
#[cfg(feature = "local-infer")]
#[test]
fn local_infer_form_accepts_all_official_packs() {
    let host = default_host_capabilities();
    for cap in ["detect.u2net", "detect.ocr", "op.generative"] {
        assert!(host.contains(&cap), "带 local-infer 的构建必须放行 {cap}（否则官方包装不进）");
    }
    assert_eq!(host.len(), KNOWN_CAPABILITIES.len(), "带 local-infer 应覆盖全部 9 项：{host:?}");

    let rows = missing_by_id();
    assert_eq!(rows.len(), 5);
    for (id, miss) in &rows {
        assert!(miss.is_empty(), "{id} 在 local-infer 形态仍缺 {miss:?} —— 官方包必须可装");
    }
}

/// 裸 web 形态（`--no-default-features`，镜像不背 ONNX/模型）→ 精确 2 包可装、3 包精确缺失。
///
/// 这是**设计内**的拒装（不是缺陷）：装上只会让每次运行 403，用户以为包坏了。
#[cfg(not(feature = "local-infer"))]
#[test]
fn bare_form_refuses_exactly_the_local_infer_packs() {
    let host = default_host_capabilities();
    for cap in ["detect.u2net", "detect.ocr", "op.generative"] {
        assert!(!host.contains(&cap), "无 local-infer 的构建不得宣称 {cap}（编译期就没有实现）");
    }
    for cap in [
        "io.read",
        "detect.white",
        "op.geometric",
        "op.overlay",
        "vl.judge",
        "report.emit",
    ] {
        assert!(host.contains(&cap), "无 local-infer 也必须支持 {cap}（纯 Rust / 远程通道）");
    }

    let rows = missing_by_id();
    assert_eq!(rows.len(), 5);
    assert_eq!(missing_of(&rows, "com.soujpg.pack.imaging"), vec!["detect.u2net"]);
    assert_eq!(missing_of(&rows, "com.soujpg.pack.segment"), vec!["detect.u2net"]);
    assert_eq!(
        missing_of(&rows, "com.soujpg.pack.ecommerce"),
        vec!["detect.ocr", "detect.u2net", "op.generative"],
        "ecommerce 的缺项集合变了 → 协议能力面或官方包声明有变动"
    );
    assert!(missing_of(&rows, "com.soujpg.pack.realestate").is_empty());
    assert!(missing_of(&rows, "com.soujpg.pack.upscale").is_empty());
}

/// 拒装文案必须可行动：点名缺哪项、本宿主有什么、以及「怎么才能装上」。
#[test]
fn capability_missing_message_tells_how_to_get_the_capability() {
    let err = RegistryError::CapabilityMissing {
        missing: vec!["detect.u2net".into()],
        host: vec!["io.read".into()],
    };
    assert_eq!(err.code(), "pack_capability_missing");
    let msg = err.to_string();
    assert!(msg.contains("detect.u2net"), "必须点名缺项：{msg}");
    assert!(msg.contains("io.read"), "必须交代本宿主支持什么：{msg}");
    assert!(
        msg.contains("local-infer"),
        "必须告诉用户怎么才能装上（local-infer 构建）：{msg}"
    );
}
