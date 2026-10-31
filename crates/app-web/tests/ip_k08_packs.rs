//! IP-K08 路由级回归钉：`.dampack` 安装 / 切指针 / 回滚 / 卸载 + 包管理端点。
//!
//! 口径（卡内用户裁决 2026-10-20）：只验**功能代码 + 单元测试**——不起真子进程做判定、
//! 不跑语料、不装真机；这里验的是「HTTP 契约 + 磁盘落地 + 账本/指针状态」三件事：
//!
//! 1. 全链路：装 v1 → 装 v2（不抢指针）→ activate → rollback → 卸载（账本行留审计）；
//! 2. 坏包三例：sha256 不符 / `api_version` 不兼容 / 能力缺项，各自**明确错误码**；
//! 3. 幂等 + 安全：同版本重复装不产生副本目录；装后被改的版本目录拒绝切换；内置包不可卸。
//!
//! 注意：app-web 依赖 `pack-host` 时 `default-features = false`（容器形态不背 ort），
//! 所以「能力缺项」这条用 settings `packs.hostCapabilities`（部署方授权子集，协议 §8）
//! 做**确定**断言，不依赖编译期特征（`cargo test --workspace` 会被 app-tauri 特征统一打开）。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn post(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

async fn delete(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

/// 响应头（弃用标记要能读）
async fn post_with_headers(
    app: &axum::Router,
    uri: &str,
    body: &str,
) -> (u16, axum::http::HeaderMap, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("ipk08-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    drop(conn);
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

fn set_json_setting<T: serde::Serialize>(data_dir: &Path, key: &str, value: &T) {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    core_storage::settings::set_json(&conn, key, value).unwrap();
}

// ---------------------------------------------------------------- 夹具：现场打 .dampack

fn demo_manifest(id: &str, version: &str, caps: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "name": "K08 夹具包",
  "version": "{version}",
  "api_version": "1.1",
  "entrypoint": {{"cmd": "python3", "args": ["plugin/__init__.py"]}},
  "capabilities": [{caps}],
  "deterministic": false,
  "timeout_secs": 30,
  "resources": ["resources/rules.json"],
  "permissions": {{"read": true, "net": false}},
  "signature": null
}}"#
    )
}

fn demo_entries(manifest: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        ("manifest.json".to_string(), manifest.as_bytes().to_vec()),
        (
            "plugin/__init__.py".to_string(),
            b"def serve():\n    pass\n".to_vec(),
        ),
        (
            "resources/rules.json".to_string(),
            r#"{"rules":[{"id":"R1","name":"白底"},{"id":"R2","name":"尺寸"}]}"#
                .as_bytes()
                .to_vec(),
        ),
        ("README.md".to_string(), b"# demo\n".to_vec()),
    ]
}

/// 打一个 `.dampack`，返回绝对路径（放在独立的 fixtures 目录，不污染 data_dir）。
fn write_pack(fixtures: &Path, name: &str, manifest: &str) -> PathBuf {
    let path = fixtures.join(name);
    let f = std::fs::File::create(&path).unwrap();
    pack_host::write_archive(f, &demo_entries(manifest)).unwrap();
    path
}

fn fixtures_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ipk08-fx-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const OK_CAPS: &str = r#""io.read", "op.geometric", "vl.judge", "report.emit""#;

// ---------------------------------------------------------------- 1. 全链路

#[tokio::test]
async fn install_activate_rollback_uninstall_lifecycle() {
    let (dir, app) = fresh_state("lifecycle");
    let fx = fixtures_root("lifecycle");
    let id = "com.example.k08.demo";
    let v1 = write_pack(&fx, "v1.dampack", &demo_manifest(id, "1.0.0", OK_CAPS));
    let v2 = write_pack(&fx, "v2.dampack", &demo_manifest(id, "2.0.0", OK_CAPS));

    // --- 装 v1：库里没有生效版本 → 自动切 current
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, v1.display()),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["id"], id);
    assert_eq!(body["version"], "1.0.0");
    assert_eq!(body["reused"], false);
    assert_eq!(body["activated"], true, "首个版本必须自动生效：{body}");
    assert_eq!(body["signatureStatus"], "unsigned");
    assert_eq!(body["currentVersion"], "1.0.0");
    let hash_v1 = body["manifestHash"].as_str().unwrap().to_string();
    assert_eq!(hash_v1.len(), 64, "manifestHash 是 sha256 全文");
    let dir_v1 = dir.join("dam").join("packs").join(id).join("1.0.0");
    assert!(dir_v1.join("manifest.json").is_file(), "版本目录必须落地");
    assert_eq!(body["dir"], dir_v1.to_string_lossy().as_ref(), "{body}");

    // --- 装 v2：已有 current → 不抢指针
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, v2.display()),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["version"], "2.0.0");
    assert_eq!(body["activated"], false, "补装新版本不该抢指针：{body}");
    assert_eq!(body["previousVersion"], "1.0.0");
    assert_eq!(body["currentVersion"], "1.0.0");

    // --- 切到 v2
    let (code, body) = post(
        &app,
        &format!("/api/compliance/packs/{id}/activate"),
        r#"{"version": "2.0.0"}"#,
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["from"], "1.0.0");
    assert_eq!(body["to"], "2.0.0");
    // 账本指针真的切了
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    use core_storage::packs::PackRepo as _;
    let cur = core_storage::packs::SqlitePackRepo::new(&conn)
        .current(id)
        .unwrap()
        .unwrap();
    assert_eq!(cur.version, "2.0.0");
    assert!(cur.current);
    drop(conn);

    // --- 回滚 = 切回上一个已装版本（旧目录保留）
    let (code, body) = post(&app, &format!("/api/compliance/packs/{id}/rollback"), "{}").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["from"], "2.0.0");
    assert_eq!(body["to"], "1.0.0");
    assert!(dir_v1.join("manifest.json").is_file(), "回滚不删旧版本目录");

    // --- 列表：版本 / 运行态 / 签名状态 / 是否内置
    let (code, body) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200, "{body}");
    let plugins = body["plugins"].as_array().expect("必须有 plugins 段");
    let me = plugins
        .iter()
        .find(|p| p["id"] == id)
        .expect("装进来的包要出现");
    assert_eq!(me["currentVersion"], "1.0.0");
    assert_eq!(me["builtin"], false);
    assert_eq!(me["versions"].as_array().unwrap().len(), 2);
    assert_eq!(me["runtime"]["available"], true, "{me}");
    assert!(body["hostCapabilities"].as_array().is_some());
    assert_eq!(body["signaturePolicy"]["required"], false);

    // --- 卸载（两个版本一起）
    let (code, body) = delete(&app, &format!("/api/compliance/packs/{id}")).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["removed"].as_array().unwrap().len(), 2);
    assert!(body["remaining"].as_array().unwrap().is_empty());
    assert_eq!(body["currentVersion"], serde_json::Value::Null);
    assert_eq!(body["staleCurrent"], "1.0.0", "指针指向已删目录要如实报出");
    assert!(!dir_v1.is_dir(), "卸载必须删版本目录");

    // --- 卸载后列表：账本行留审计（present=false），currentVersion 归 null
    let (code, body) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200);
    let me = body["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .expect("审计行还在")
        .clone();
    assert_eq!(me["currentVersion"], serde_json::Value::Null);
    assert!(me["versions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v["present"] == false));

    // --- 再切/再回滚：包没了就必须明确报错，不许假装成功
    let (code, body) = post(&app, &format!("/api/compliance/packs/{id}/activate"), "{}").await;
    assert_eq!(code, 404, "{body}");
    assert_eq!(body["error"], "pack_not_installed");
}

// ---------------------------------------------------------------- 2. 上传通道 + 幂等

#[tokio::test]
async fn install_from_base64_upload_is_idempotent() {
    let (dir, app) = fresh_state("upload");
    let id = "com.example.k08.upload";
    let manifest = demo_manifest(id, "1.0.0", OK_CAPS);
    // 直接内存打包 → base64（带 data URL 前缀，模拟浏览器 FileReader）
    let mut buf = std::io::Cursor::new(Vec::new());
    pack_host::write_archive(&mut buf, &demo_entries(&manifest)).unwrap();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    let body = format!(
        r#"{{"archiveBase64": "data:application/zip;base64,{b64}", "filename": "demo.dampack"}}"#
    );

    let (code, out) = post(&app, "/api/compliance/packs/install", &body).await;
    assert_eq!(code, 200, "{out}");
    assert_eq!(out["source"], "upload");
    assert_eq!(out["filename"], "demo.dampack");
    assert_eq!(out["reused"], false);

    // 幂等：同版本同内容再装 → reused=true，磁盘上仍只有一个版本目录
    let (code, out) = post(&app, "/api/compliance/packs/install", &body).await;
    assert_eq!(code, 200, "{out}");
    assert_eq!(out["reused"], true, "同版本同内容必须复用：{out}");
    let pack_root = dir.join("dam").join("packs").join(id);
    let dirs: Vec<_> = std::fs::read_dir(&pack_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "不得产生第二个副本目录：{dirs:?}");
    // 也不留暂存目录
    let staging: Vec<_> = std::fs::read_dir(dir.join("dam").join("packs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
        .collect();
    assert!(staging.is_empty(), "暂存目录必须清理：{staging:?}");
}

// ---------------------------------------------------------------- 3. 坏包三例 + 签名 + 篡改

#[tokio::test]
async fn bad_packs_report_specific_error_codes() {
    let (_dir, app) = fresh_state("badpacks");
    let fx = fixtures_root("badpacks");
    let id = "com.example.k08.bad";

    // (a) sha256 不符：SHA256SUMS 里的 manifest 摘要是假的
    let mut entries = demo_entries(&demo_manifest(id, "1.0.0", OK_CAPS));
    entries.push((
        "SHA256SUMS".to_string(),
        format!("{}  manifest.json\n", "0".repeat(64)).into_bytes(),
    ));
    let bad_sha = fx.join("bad-sha.dampack");
    pack_host::write_archive(std::fs::File::create(&bad_sha).unwrap(), &entries).unwrap();
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, bad_sha.display()),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_digest_mismatch", "{body}");

    // (b) api_version 不兼容（宿主严格相等 1.1）
    let bad_api = write_pack(
        &fx,
        "bad-api.dampack",
        &demo_manifest(id, "1.0.0", OK_CAPS)
            .replace("\"api_version\": \"1.1\"", "\"api_version\": \"1.0\""),
    );
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, bad_api.display()),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_api_version", "{body}");
    assert!(body["message"].as_str().unwrap().contains("1.0"), "{body}");

    // (c) 能力缺项：部署方只授权 io.read（协议 §8 的「子集」），包还要 detect.ocr
    set_json_setting(&_dir, "packs.hostCapabilities", &vec!["io.read"]);
    let need_ocr = write_pack(
        &fx,
        "need-ocr.dampack",
        &demo_manifest(id, "1.0.0", r#""io.read", "detect.ocr""#),
    );
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, need_ocr.display()),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_capability_missing", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("detect.ocr"),
        "{body}"
    );

    // 三个坏包都不许留下任何版本目录（失败不能半装）
    let root = _dir.join("dam").join("packs").join(id);
    assert!(!root.is_dir(), "坏包不许落盘：{root:?}");
}

#[tokio::test]
async fn trusted_keys_make_signature_mandatory() {
    let (dir, app) = fresh_state("sigpolicy");
    let fx = fixtures_root("sigpolicy");
    // 任意 32 字节公钥（内容不重要：这里只验「配了信任公钥 = 强制验签」的策略语义）
    set_json_setting(&dir, "packs.trustedKeys", &vec!["11".repeat(32)]);
    let unsigned = write_pack(
        &fx,
        "unsigned.dampack",
        &demo_manifest("com.example.k08.sig", "1.0.0", OK_CAPS),
    );
    let (code, body) = post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, unsigned.display()),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_signature_required", "{body}");

    // 列表里如实报「验签已启用 + 信任公钥条数」
    let (code, body) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200);
    assert_eq!(body["signaturePolicy"]["required"], true);
    assert_eq!(body["signaturePolicy"]["trustedKeys"], 1);
}

#[tokio::test]
async fn tampered_version_dir_is_refused_on_activate() {
    let (dir, app) = fresh_state("tamper");
    let fx = fixtures_root("tamper");
    let id = "com.example.k08.tamper";
    let v1 = write_pack(&fx, "v1.dampack", &demo_manifest(id, "1.0.0", OK_CAPS));
    let v2 = write_pack(&fx, "v2.dampack", &demo_manifest(id, "2.0.0", OK_CAPS));
    for p in [&v1, &v2] {
        let (code, body) = post(
            &app,
            "/api/compliance/packs/install",
            &format!(r#"{{"path": "{}"}}"#, p.display()),
        )
        .await;
        assert_eq!(code, 200, "{body}");
    }
    // 直接改已装版本目录的 manifest（合法 JSON，但内容变了 → 与账本登记不符）
    let mf = dir
        .join("dam")
        .join("packs")
        .join(id)
        .join("2.0.0")
        .join("manifest.json");
    let text = std::fs::read_to_string(&mf).unwrap();
    std::fs::write(&mf, text.replace("K08 夹具包", "被改过的包")).unwrap();
    let (code, body) = post(
        &app,
        &format!("/api/compliance/packs/{id}/activate"),
        r#"{"version": "2.0.0"}"#,
    )
    .await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["error"], "pack_tampered", "{body}");
}

// ---------------------------------------------------------------- 4. 内置保护 / 版本指针保护 / 参数错

#[tokio::test]
async fn builtin_and_current_version_are_protected() {
    let (_dir, app) = fresh_state("protect");
    // 内置包（电商插件）不可卸载
    let (code, body) = delete(&app, "/api/compliance/packs/com.soujpg.pack.ecommerce").await;
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["error"], "pack_builtin_protected", "{body}");

    // 正在生效的版本不许单独卸
    let fx = fixtures_root("protect");
    let id = "com.example.k08.protect";
    let v1 = write_pack(&fx, "v1.dampack", &demo_manifest(id, "1.0.0", OK_CAPS));
    let v2 = write_pack(&fx, "v2.dampack", &demo_manifest(id, "2.0.0", OK_CAPS));
    for p in [&v1, &v2] {
        post(
            &app,
            "/api/compliance/packs/install",
            &format!(r#"{{"path": "{}"}}"#, p.display()),
        )
        .await;
    }
    let (code, _) = post(
        &app,
        &format!("/api/compliance/packs/{id}/activate"),
        r#"{"version": "2.0.0"}"#,
    )
    .await;
    assert_eq!(code, 200);
    let (code, body) = delete(&app, &format!("/api/compliance/packs/{id}?version=2.0.0")).await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["error"], "pack_version_current", "{body}");

    // 未装版本 → 明确报错
    let (code, body) = post(
        &app,
        &format!("/api/compliance/packs/{id}/activate"),
        r#"{"version": "9.9.9"}"#,
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_version_not_installed", "{body}");

    // 只有一个版本时回滚 → 没有更早版本
    let one = "com.example.k08.single";
    let p = write_pack(&fx, "one.dampack", &demo_manifest(one, "1.0.0", OK_CAPS));
    post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, p.display()),
    )
    .await;
    let (code, body) = post(&app, &format!("/api/compliance/packs/{one}/rollback"), "{}").await;
    assert_eq!(code, 404, "{body}");
    assert_eq!(body["error"], "pack_no_previous_version", "{body}");

    // 缺参数（既没 path 也没 archiveBase64）
    let (code, body) = post(&app, "/api/compliance/packs/install", "{}").await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "pack_install_args", "{body}");
}

// ---------------------------------------------------------------- 5. 详情 + 旧导入的弃用标记

#[tokio::test]
async fn plugin_detail_and_legacy_import_deprecation() {
    let (_dir, app) = fresh_state("detail");
    let fx = fixtures_root("detail");
    let id = "com.example.k08.detail";
    let v1 = write_pack(&fx, "v1.dampack", &demo_manifest(id, "1.0.0", OK_CAPS));
    post(
        &app,
        "/api/compliance/packs/install",
        &format!(r#"{{"path": "{}"}}"#, v1.display()),
    )
    .await;

    // GET /packs/{id}：manifest + 规则文档（静态读 resources/rules.json，不拉子进程）
    let (code, body) = get(&app, &format!("/api/compliance/packs/{id}?probe=false")).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["source"], "plugin");
    assert_eq!(body["manifest"]["id"], id);
    assert_eq!(body["manifest"]["version"], "1.0.0");
    assert_eq!(body["active"], true);
    assert_eq!(body["rules"].as_array().unwrap().len(), 2, "{body}");
    assert_eq!(
        body["packInfo"],
        serde_json::Value::Null,
        "不 probe 就不起子进程"
    );
    assert!(body["versions"].as_array().is_some());

    // IP-K16：旧「配置包（四件 YAML）导入」端点已下线 —— 装包只有 .dampack 一条路
    let (code, _headers, _body) = post_with_headers(
        &app,
        "/api/compliance/packs/import",
        r#"{"dir": "/tmp/whatever"}"#,
    )
    .await;
    assert!(code == 404 || code == 405, "旧导入端点必须已下线：{code}");
}
