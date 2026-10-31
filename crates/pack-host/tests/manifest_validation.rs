//! `manifest.json` 校验的**拒装面**逐条单测（协议 §4.0 / §4.1 / §8）。
//!
//! 为什么值得单列一文件：manifest 是唯一配置面，写错键名/漏字段/越界若被静默接受，
//! 后果不是「报错」而是「按错误配置跑完全库」。所以每条拒装条件都要有一个
//! 「能拒 + 错误里点名是哪一栏」的断言，而不是只测 happy path。
//!
//! 全部纯内存/临时目录，无子进程、无模型，毫秒级。

use std::path::Path;

use pack_host::manifest::{
    load_pack, Manifest, ManifestError, PackIdentity, API_VERSION, KNOWN_CAPABILITIES,
};
use serde_json::{json, Value};

fn good() -> Value {
    json!({
        "id": "test.manifest",
        "name": "清单校验夹具",
        "version": "1.0.0",
        "api_version": "1.1",
        "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
        "capabilities": ["io.read", "op.geometric"],
        "deterministic": true,
        "timeout_secs": 30
    })
}

fn parse(v: &Value) -> Result<Manifest, ManifestError> {
    Manifest::from_slice(v.to_string().as_bytes())
}

fn err_of(v: &Value) -> String {
    match parse(v) {
        Ok(m) => panic!("本应拒装，却解析成功：{m:?}"),
        Err(e) => e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 1. 必填字段 / 空值
// ---------------------------------------------------------------------------

#[test]
fn required_fields_are_not_optional() {
    // 协议面 {id,name,version,api_version,entrypoint,deterministic,timeout_secs} 缺一即拒装
    for field in [
        "id",
        "name",
        "version",
        "api_version",
        "entrypoint",
        "deterministic",
        "timeout_secs",
    ] {
        let mut v = good();
        v.as_object_mut().unwrap().remove(field);
        let msg = err_of(&v);
        assert!(msg.contains(field), "缺 {field} 时错误必须点名它：{msg}");
    }
}

#[test]
fn blank_identity_fields_are_rejected_with_field_name() {
    for field in ["id", "name", "version"] {
        for blank in ["", "   "] {
            let mut v = good();
            v[field] = json!(blank);
            let msg = err_of(&v);
            assert!(msg.contains(field), "{field}={blank:?} → {msg}");
            assert!(msg.contains("不能为空"), "{field}={blank:?} → {msg}");
        }
    }
}

#[test]
fn entrypoint_cmd_must_be_non_empty() {
    let mut v = good();
    v["entrypoint"]["cmd"] = json!("");
    let msg = err_of(&v);
    assert!(msg.contains("entrypoint.cmd"), "{msg}");
}

#[test]
fn entrypoint_args_must_stay_inside_pack() {
    // 绝对路径 = 逃出包根（例如 args: ["/etc/passwd"]），必须拒
    let mut v = good();
    v["entrypoint"]["args"] = json!(["plugin/__init__.py", "/usr/bin/evil.py"]);
    let msg = err_of(&v);
    assert!(msg.contains("相对包根"), "{msg}");
    assert!(msg.contains("/usr/bin/evil.py"), "{msg}");
}

// ---------------------------------------------------------------------------
// 2. 未知键 = 拒装（顶层 + 嵌套）
// ---------------------------------------------------------------------------

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    // 顶层：这是「写错键名却静默生效」的主要风险面
    let mut v = good();
    v["fixPolicy"] = json!("generative");
    let msg = err_of(&v);
    assert!(msg.contains("fixPolicy"), "{msg}");

    // entrypoint 内层
    let mut v = good();
    v["entrypoint"]["timeout"] = json!(5);
    let msg = err_of(&v);
    assert!(msg.contains("timeout"), "{msg}");

    // permissions 内层（缺省对象也必须严格）
    let mut v = good();
    v["permissions"] = json!({"read": true, "write": true});
    let msg = err_of(&v);
    assert!(msg.contains("write"), "{msg}");
}

#[test]
fn permissions_defaults_and_explicit_values() {
    // 缺省 = {read:true, net:false}（协议 §4.0）
    let m = parse(&good()).unwrap();
    assert!(m.permissions.read);
    assert!(!m.permissions.net);

    // 显式声明按声明走（net=true 是允许的，只是默认关闭）
    let mut v = good();
    v["permissions"] = json!({"read": false, "net": true});
    let m = parse(&v).unwrap();
    assert!(!m.permissions.read);
    assert!(m.permissions.net);

    // 部分字段 → 其余取默认
    let mut v = good();
    v["permissions"] = json!({"net": true});
    let m = parse(&v).unwrap();
    assert!(m.permissions.read, "read 缺省应为 true");
    assert!(m.permissions.net);
}

#[test]
fn signature_is_optional_but_type_checked() {
    let m = parse(&good()).unwrap();
    assert!(m.signature.is_none());

    let mut v = good();
    v["signature"] = json!("sha256:deadbeef");
    assert_eq!(
        parse(&v).unwrap().signature.as_deref(),
        Some("sha256:deadbeef")
    );

    let mut v = good();
    v["signature"] = json!(12345);
    assert!(parse(&v).is_err(), "signature 必须是字符串");
}

// ---------------------------------------------------------------------------
// 3. api_version 严格相等（不做兼容层）
// ---------------------------------------------------------------------------

#[test]
fn api_version_must_match_exactly() {
    // 只接受 protocol 冻结的 1.1：1.1.0 / 1.0 / 0.9 / 空串 一律拒
    for bad in ["1.1.0", "1.0", "0.9", "", " 1.1"] {
        let mut v = good();
        v["api_version"] = json!(bad);
        match parse(&v) {
            Err(ManifestError::IncompatibleApiVersion { found, expected }) => {
                assert_eq!(found, bad);
                assert_eq!(expected, API_VERSION);
            }
            other => panic!("api_version={bad:?} 应判不兼容，实得 {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 4. timeout / capabilities 边界
// ---------------------------------------------------------------------------

#[test]
fn timeout_bounds_are_inclusive() {
    for ok in [1u64, 3600] {
        let mut v = good();
        v["timeout_secs"] = json!(ok);
        assert_eq!(parse(&v).unwrap().timeout_secs, ok, "{ok} 应合法");
    }
    for bad in [0u64, 3601, u64::MAX] {
        let mut v = good();
        v["timeout_secs"] = json!(bad);
        assert!(err_of(&v).contains("timeout_secs"), "{bad}");
    }
    // 负数/浮点不是 u64 → 解析期就拒
    for bad in [json!(-1), json!(1.5), json!("60")] {
        let mut v = good();
        v["timeout_secs"] = bad.clone();
        assert!(parse(&v).is_err(), "timeout_secs={bad} 应拒");
    }
}

#[test]
fn capabilities_whitelist_is_exact_and_deduped() {
    // 白名单闭集就是协议 §6 的 9 项（N6c 追加 `op.overlay` = `/op/watermark`）
    assert_eq!(
        KNOWN_CAPABILITIES,
        [
            "io.read",
            "detect.u2net",
            "detect.ocr",
            "detect.white",
            "op.geometric",
            "op.generative",
            "op.overlay",
            "vl.judge",
            "report.emit",
        ]
    );

    let mut v = good();
    v["capabilities"] = json!(KNOWN_CAPABILITIES);
    assert_eq!(parse(&v).unwrap().capabilities.len(), 9);

    // 多个未知能力：错误里点名、且去重后列一次
    let mut v = good();
    v["capabilities"] = json!(["io.read", "op.telepathy", "op.telepathy", "x.y"]);
    let msg = err_of(&v);
    assert!(msg.contains("op.telepathy"), "{msg}");
    assert!(msg.contains("x.y"), "{msg}");
    assert_eq!(
        msg.matches("op.telepathy").count(),
        1,
        "未知项应去重：{msg}"
    );

    // 大小写不宽松：IO.READ ≠ io.read（白名单是字面量匹配）
    let mut v = good();
    v["capabilities"] = json!(["IO.READ"]);
    assert!(parse(&v).is_err());
}

// ---------------------------------------------------------------------------
// 5. 装机期存在性校验（resources / entrypoint 文件）
// ---------------------------------------------------------------------------

#[test]
fn install_time_existence_checks_report_missing_items_only() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("pack");
    std::fs::create_dir_all(pack.join("plugin")).unwrap();
    std::fs::create_dir_all(pack.join("resources")).unwrap();
    std::fs::write(pack.join("plugin/__init__.py"), b"").unwrap();
    std::fs::write(pack.join("resources/rules.json"), b"{}").unwrap();

    let mut v = good();
    v["resources"] = json!(["resources/rules.json", "resources/taxonomy.json"]);
    let m = parse(&v).unwrap();

    // resources：存在的项不报，缺的按 manifest 顺序列出
    assert_eq!(
        m.missing_resources(&pack),
        vec!["resources/taxonomy.json".to_string()]
    );
    // entrypoint.args：plugin/__init__.py 在盘 → 无缺项
    assert!(m.missing_entrypoint_files(&pack).is_empty());

    // 入口缺失也要能报（安装期就能拦住「包装上了但起不来」）
    std::fs::remove_file(pack.join("plugin/__init__.py")).unwrap();
    assert_eq!(
        m.missing_entrypoint_files(&pack),
        vec!["plugin/__init__.py".to_string()]
    );
    // 全部存在时恒为空
    std::fs::write(pack.join("resources/taxonomy.json"), b"{}").unwrap();
    assert!(m.missing_resources(&pack).is_empty());
}

// ---------------------------------------------------------------------------
// 6. load_pack：IO 错误 + pluginHash 语义
// ---------------------------------------------------------------------------

#[test]
fn load_pack_reports_missing_manifest_with_path() {
    let dir = tempfile::tempdir().unwrap();
    match load_pack(dir.path()) {
        Err(ManifestError::Io { path, .. }) => {
            assert_eq!(
                path,
                dir.path().join("manifest.json"),
                "错误必须点名期望路径"
            );
        }
        other => panic!("期望 Io 错误，实得 {other:?}"),
    }
    // 目录存在但 manifest 是坏 JSON
    std::fs::write(dir.path().join("manifest.json"), b"{oops").unwrap();
    assert!(matches!(load_pack(dir.path()), Err(ManifestError::Json(_))));
}

#[test]
fn plugin_hash_covers_raw_bytes_not_parsed_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.json");
    // 同一份语义、不同空白 = 不同 pluginHash（§7 铁律 7：改一字节必须升 version）
    let pretty = serde_json::to_vec_pretty(&good()).unwrap();
    std::fs::write(&path, &pretty).unwrap();
    let (_, identity) = load_pack(dir.path()).unwrap();
    assert_eq!(identity.plugin_hash, core_hash::sha256_hex(&pretty));
    assert_eq!(
        identity.label(),
        format!(
            "test.manifest@1.0.0#{}",
            &core_hash::sha256_hex(&pretty)[..12]
        )
    );

    let compact = good().to_string().into_bytes();
    std::fs::write(&path, &compact).unwrap();
    let (_, second) = load_pack(dir.path()).unwrap();
    assert_ne!(
        second.plugin_hash, identity.plugin_hash,
        "空白变化必须改变 pluginHash"
    );
    assert_eq!(second.plugin_hash, core_hash::sha256_hex(&compact));
}

#[test]
fn pack_label_truncates_short_hashes_safely() {
    // 正常 64 位 hex → 12 位
    let full = PackIdentity {
        id: "a.b".into(),
        version: "1.0.0".into(),
        plugin_hash: "0123456789abcdef".to_string(),
    };
    assert_eq!(full.label(), "a.b@1.0.0#0123456789ab");
    // 异常短 hash（脏数据）不得 panic（切片边界）
    let short = PackIdentity {
        id: "a.b".into(),
        version: "1.0.0".into(),
        plugin_hash: "abc".into(),
    };
    assert_eq!(short.label(), "a.b@1.0.0#abc");
    let empty = PackIdentity {
        id: "a.b".into(),
        version: "1.0.0".into(),
        plugin_hash: String::new(),
    };
    assert_eq!(empty.label(), "a.b@1.0.0#");
}

// ---------------------------------------------------------------------------
// 7. 参考包 / 官方包必须真实可载入（拒装面之外的「装得进去」）
// ---------------------------------------------------------------------------

#[test]
fn shipped_packs_load_and_pass_install_time_checks() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let packs = [
        repo.join("tools/pack-conformance/fixtures/good"),
        repo.join("packs/plugins/ecommerce"),
    ];
    for pack in packs {
        let (manifest, identity) =
            load_pack(&pack).unwrap_or_else(|e| panic!("{} 载入失败：{e}", pack.display()));
        assert_eq!(manifest.api_version, API_VERSION);
        assert!(
            manifest.missing_resources(&pack).is_empty(),
            "{} 声明了不存在的 resources：{:?}",
            pack.display(),
            manifest.missing_resources(&pack)
        );
        assert!(
            manifest.missing_entrypoint_files(&pack).is_empty(),
            "{} 的 entrypoint.args 指到了不存在的文件：{:?}",
            pack.display(),
            manifest.missing_entrypoint_files(&pack)
        );
        assert_eq!(
            identity.plugin_hash.len(),
            64,
            "pluginHash 必须是 sha256 hex"
        );
    }
}
