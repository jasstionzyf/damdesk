//! N6d `t_979f61d2` 装配的**路由级**验收：装包 → schema 接口 → 列表唯一 → 卸载/重装。
//!
//! 卡面验收原文（第 1、3 条）：
//!   - 安装后 `GET /api/plugins/imaging/schema` 返回三组参数（贴原文）；插件列表里**只有一个** imaging 插件；
//!   - 卸载重装仍可用。
//!
//! 本文件走真 `app_web::router`（`router_without_dist`）+ 真 SQLite + 真 `PackStore`，
//! 只把 `pack_host` 的子进程执行换成不执行（这些路由都不启插件进程：
//! schema 是「读包内文件」的廉价接口，packs 列表读账本）。
//!
//! 不在这里验的（见报告 §7）：宿主侧**动作链**编排 —— 当前 `app-web`/`svc-pipeline` 里
//! 没有 `actions[]` 也没有链式调用实现（`grep -rn actions crates/app-web/src crates/svc-pipeline/src`
//! 无命中），一条 job 串 resize→convert→watermark 属于宿主能力缺口，不在装配卡边界内。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

const PACK_ID: &str = "com.soujpg.pack.imaging";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn pack_dir() -> PathBuf {
    repo_root().join("packs/plugins/imaging")
}

fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("n6d-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    drop(core_storage::open(&db).unwrap());
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

async fn send(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 24).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// 把仓库里的包目录打成 `.dampack`（与 `scripts/k15_build_packs.py` 同布局：
/// 条目在归档根、不打 `tests/`）。装包接口收的是**归档**，不是目录（传目录会 400 `pack_archive`）。
fn build_archive(out_dir: &Path) -> PathBuf {
    build_archive_as(out_dir, PACK_ID)
}

/// 以**指定 id** 打包（真源仍是仓库 `packs/plugins/imaging`，只把 manifest.json 的 id 改掉）。
///
/// 为什么需要（N15 合并后实测）：N9/KI-31 给 `plugin_pack_dir_for` 加了「仓库内置包」兜底
/// （`repo_builtin_pack_dir`，dev/`cargo test` 形态按 manifest id 命中任意 `packs/plugins/<dir>`），
/// 于是卸载 imaging 包后 schema 仍能从仓库目录读到 → 200。发行形态 `env!("CARGO_MANIFEST_DIR")`
/// 指向构建机、兜底自然为 None，卸载后仍应 404。要验「卸载 → 404」这条产品语义而不受 dev 兜底影响，
/// 就用一个**仓库里不存在的 id** 装同一份包。
fn build_archive_as(out_dir: &Path, id: &str) -> PathBuf {
    let src = pack_dir();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for item in std::fs::read_dir(dir).expect("读包目录") {
            let item = item.expect("目录项");
            let path = item.path();
            let rel = path
                .strip_prefix(base)
                .expect("相对路径")
                .to_string_lossy()
                .replace('\\', "/");
            if rel.split('/').any(|part| part == "tests" || part == "__pycache__") {
                continue;
            }
            if path.is_dir() {
                walk(&path, base, out);
            } else if path.is_file() {
                out.push((rel, std::fs::read(&path).expect("读文件")));
            }
        }
    }
    walk(&src, &src, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(entries.iter().any(|(n, _)| n == "manifest.json"), "归档必须含 manifest.json");
    if id != PACK_ID {
        let pos = entries
            .iter()
            .position(|(n, _)| n == "manifest.json")
            .expect("manifest 位置");
        let mut manifest: Value =
            serde_json::from_slice(&entries[pos].1).expect("仓库 manifest 必须是合法 JSON");
        manifest["id"] = Value::String(id.to_string());
        entries[pos].1 = serde_json::to_vec_pretty(&manifest).expect("重写 manifest");
    }

    std::fs::create_dir_all(out_dir).expect("建归档目录");
    let target = out_dir.join(format!("{id}-2.0.0.dampack"));
    let file = std::fs::File::create(&target).expect("建归档");
    pack_host::write_archive(file, &entries).expect("写归档失败");
    target
}

async fn install(app: &axum::Router, archive: &Path) -> (u16, Value) {
    send(
        app,
        "POST",
        "/api/compliance/packs/install",
        Some(serde_json::json!({"path": archive.to_string_lossy(), "activate": true})),
    )
    .await
}

fn imaging_entries(list: &Value) -> Vec<Value> {
    list.get("plugins")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("id").and_then(Value::as_str) == Some(PACK_ID))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 「真正装着的版本」= `versions[].present` 为真的那些。
///
/// 卸载**不删账本行**（`pack_uninstall` 的语义：账本作审计保留，已删目录的版本不再算已安装），
/// 所以「卸载成功」的判据是「没有 present 的版本」，而不是「列表里没有这个 id」。
fn present_versions(list: &Value) -> Vec<String> {
    imaging_entries(list)
        .iter()
        .flat_map(|entry| {
            entry
                .get("versions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|v| v.get("present").and_then(Value::as_bool) == Some(true))
        .filter_map(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 验收 1：装得上、schema 接口吐四组（basic + 三组参数）、列表里只有一个 imaging。
#[tokio::test]
async fn install_then_schema_endpoint_and_plugin_list() {
    let (dir, app) = fresh_state("install");
    let archive = build_archive(&dir);

    let (code, body) = install(&app, &archive).await;
    assert_eq!(code, 200, "装包失败：{body}");
    assert_eq!(body["id"], PACK_ID, "{body}");
    assert_eq!(body["version"], "2.0.0", "{body}");

    // ---- schema 接口（卡面要求「贴原文」：整段打到 stdout，报告里粘的就是它）
    let (code, schema_body) = send(&app, "GET", &format!("/api/plugins/{PACK_ID}/schema"), None).await;
    assert_eq!(code, 200, "schema 接口失败：{schema_body}");
    println!(
        "---- GET /api/plugins/{PACK_ID}/schema ----\n{}",
        serde_json::to_string_pretty(&schema_body).unwrap()
    );
    assert_eq!(schema_body["pluginId"], PACK_ID);
    assert_eq!(schema_body["declared"], true, "必须声明参数 schema：{schema_body}");
    assert_eq!(schema_body["schemaFile"], "resources/params.schema.json");
    assert_eq!(
        schema_body["pluginHash"].as_str().map(str::len),
        Some(64),
        "身份哈希要是 manifest 原文 sha256 全文"
    );
    let schema = &schema_body["schema"];
    assert_eq!(schema["schemaVersion"], 1, "{schema_body}");

    // 分组顺序 = 界面分区顺序：basic（动作）→ geometry → codec → overlay
    let groups: Vec<&str> = schema["groups"]
        .as_array()
        .expect("groups 必须是数组")
        .iter()
        .map(|g| g["key"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(groups, vec!["basic", "geometry", "codec", "overlay"], "{schema_body}");

    // 动作参数：六个动作、默认 resize（装配层「一个插件多动作」的入口）
    let action = &schema["properties"]["action"];
    assert_eq!(action["type"], "enum", "{action}");
    assert_eq!(
        action["options"],
        serde_json::json!(["resize", "crop", "convert", "compress", "watermark", "rename"]),
        "{action}"
    );
    assert_eq!(action["default"], "resize");
    assert_eq!(action["group"], "basic");

    // 三组参数都在（各取一个代表键，证明不是只透了一组）
    for (key, group) in [
        ("resizeLongEdge", "geometry"),
        ("compressQuality", "codec"),
        ("watermarkText", "overlay"),
    ] {
        assert_eq!(
            schema["properties"][key]["group"], group,
            "参数 {key} 必须在 {group} 组：{schema_body}"
        );
    }
    // 卡面硬边界：不许有跨组 when（`action` 不得出现在任何参数的 when 里）
    for (key, spec) in schema["properties"].as_object().expect("properties") {
        if let Some(when) = spec.get("when").and_then(Value::as_object) {
            assert!(
                !when.contains_key("action"),
                "参数 {key} 对 action 加了跨组门控（卡面明确禁止）：{spec}"
            );
        }
    }

    // ---- 插件列表：只有一个 imaging 包
    let (code, list) = send(&app, "GET", "/api/compliance/packs", None).await;
    assert_eq!(code, 200, "{list}");
    let rows = imaging_entries(&list);
    assert_eq!(rows.len(), 1, "列表里应只有一个 imaging 包：{list}");
    assert_eq!(rows[0]["currentVersion"], "2.0.0", "activate=true 后应生效：{list}");
    assert_eq!(present_versions(&list), vec!["2.0.0"], "{list}");
}

/// 验收 3：卸载 → 列表里没有 → schema 404 → 重装仍可用（且能再卸载干净）。
#[tokio::test]
async fn uninstall_and_reinstall_round_trip() {
    let (dir, app) = fresh_state("reinstall");
    let archive = build_archive(&dir);
    assert_eq!(install(&app, &archive).await.0, 200);

    let (code, body) = send(&app, "DELETE", &format!("/api/compliance/packs/{PACK_ID}"), None).await;
    assert_eq!(code, 200, "卸载失败：{body}");
    let (_, list) = send(&app, "GET", "/api/compliance/packs", None).await;
    assert!(
        present_versions(&list).is_empty(),
        "卸载后不该还有已安装版本（账本行保留是设计）：{list}"
    );

    // 卸载后 schema：dev/`cargo test` 形态会被「仓库内置包」兜底解析到 `packs/plugins/imaging` → 200。
    // 这条兜底是 N9/KI-31 为「单机开发态不装包也能跑多行业包」加的，发行形态（`CARGO_MANIFEST_DIR`
    // 指向构建机、不存在）自然失效。真正的产品语义「卸载 → 404」用下面的非内置 id 用例钉住。
    let (code, body) = send(&app, "GET", &format!("/api/plugins/{PACK_ID}/schema"), None).await;
    assert_eq!(code, 200, "dev 形态应回落到仓库内置包：{body}");
    let builtin_hash = body["pluginHash"].as_str().unwrap_or("").to_string();

    // 重装
    let (code, body) = install(&app, &archive).await;
    assert_eq!(code, 200, "重装失败：{body}");
    let (code, schema_body) = send(&app, "GET", &format!("/api/plugins/{PACK_ID}/schema"), None).await;
    assert_eq!(code, 200, "重装后 schema 打不开：{schema_body}");
    assert_eq!(schema_body["declared"], true);
    let (_, list) = send(&app, "GET", "/api/compliance/packs", None).await;
    assert_eq!(imaging_entries(&list).len(), 1, "{list}");
    assert_eq!(present_versions(&list), vec!["2.0.0"], "重装后应真的有该版本：{list}");
    let _ = builtin_hash;
}

/// 验收 3（产品语义）：**卸载 → schema 404**。
///
/// 用仓库里不存在的 pack id 装同一份包：没有「仓库内置」兜底参与解析，
/// 卸载后 `plugin_pack_dir_for` 再也找不到目录 → 404 `plugin_not_found`（界面据此显示「未安装」）。
#[tokio::test]
async fn uninstall_then_schema_404_for_non_builtin_id() {
    const SIDE_ID: &str = "com.soujpg.pack.imaging.n15";
    let (dir, app) = fresh_state("reinstall-side");
    let archive = build_archive_as(&dir, SIDE_ID);

    let (code, body) = install(&app, &archive).await;
    assert_eq!(code, 200, "装包失败：{body}");
    assert_eq!(body["id"], SIDE_ID, "{body}");

    let (code, body) = send(&app, "GET", &format!("/api/plugins/{SIDE_ID}/schema"), None).await;
    assert_eq!(code, 200, "装上后 schema 必须可读：{body}");
    assert_eq!(body["pluginId"], SIDE_ID, "{body}");
    assert_eq!(body["declared"], true, "{body}");

    let (code, body) = send(&app, "DELETE", &format!("/api/compliance/packs/{SIDE_ID}"), None).await;
    assert_eq!(code, 200, "卸载失败：{body}");

    let (code, body) = send(&app, "GET", &format!("/api/plugins/{SIDE_ID}/schema"), None).await;
    assert_eq!(code, StatusCode::NOT_FOUND.as_u16(), "卸载后 schema 应 404：{body}");

    // 重装仍可用
    let (code, body) = install(&app, &archive).await;
    assert_eq!(code, 200, "重装失败：{body}");
    let (code, body) = send(&app, "GET", &format!("/api/plugins/{SIDE_ID}/schema"), None).await;
    assert_eq!(code, 200, "重装后 schema 打不开：{body}");
}

/// 反面：宿主没有 `detect.u2net`（web 容器构建 / 关掉了本地推理）→ 装包必须被**正确拒装**，
/// 而不是装上一个 crop 永远 403 的包。这条同时锁住「manifest 的能力声明是真约束」。
#[tokio::test]
async fn install_rejected_when_host_lacks_u2net() {
    let (dir, app) = fresh_state("no-u2net");
    let archive = build_archive(&dir);
    {
        let conn = core_storage::open(&dir.join("dam.db")).unwrap();
        core_storage::settings::set_json(
            &conn,
            app_web::compliance_api::PACK_HOST_CAPS_KEY,
            &serde_json::json!(["io.read", "detect.white", "op.geometric", "op.overlay"]),
        )
        .unwrap();
    }
    let (code, body) = install(&app, &archive).await;
    assert_eq!(code, 400, "宿主能力不齐必须拒装：{body}");
    assert_eq!(body["error"], "pack_capability_missing", "{body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("detect.u2net"),
        "拒装理由必须点名缺哪个能力：{body}"
    );
    // 拒装后列表里不该有它（不许「装了但跑不了」）
    let (_, list) = send(&app, "GET", "/api/compliance/packs", None).await;
    assert!(present_versions(&list).is_empty(), "拒装后不该有已安装版本：{list}");
}
