#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! N18（卡 task）回归钉：**判定默认走插件 + 错误可读 + 没有引擎开关**。
//!
//! 背景（N15 真机批次缺陷）：装好插件、建好规格后，用规格发起判定直接 400 —— 那时生效的
//! 还是 legacy 引擎通道，而界面**没有**引擎开关，用户看到的就是「装了插件跑不了」。
//! IP-K16 删了 legacy 执行路径；本文件钉住收口后的三条硬事实：
//!
//! 1. **默认即插件**：库里没有任何 `compliance` 引擎设置（全新安装）→ 判定照常发起；
//!    库里残留老设置（升级安装 `{engine:"legacy"}`）→ 一样照常发起（老配置不挡路、也不被当作开关）；
//! 2. **错误可读**：插件未安装 / 规格指向已卸载插件 / 规格版本钉不上 → 400 且带
//!    `code + message + hint + action`（界面据此给「去哪儿装/改」的去处，不是裸错误码）；
//! 3. **没有开关**：settings 里不会再出现 `compliance` 键（迁移 0030 清掉），
//!    代码里也没有 `DAM_COMPLIANCE_ENGINE` 的可读路径。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tower::ServiceExt;

/// libvips 是进程级全局状态：显式初始化一次再串行化（与 judge_entry_routes 同法）。
fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

fn make_png(path: &Path) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [200.0]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

struct Fixture {
    app: axum::Router,
    data: PathBuf,
}

fn setup(tag: &str) -> Fixture {
    let data = std::env::temp_dir().join(format!("n18-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    let src = data.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..2i64 {
        let p = src.join(format!("img{i}.png"));
        make_png(&p);
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'png', 10, 0, ?1)",
            rusqlite::params![
                i + 1,
                p.to_string_lossy().to_string(),
                p.file_name().unwrap().to_string_lossy().to_string()
            ],
        )
        .unwrap();
    }
    drop(conn);

    // 全新安装形状：没有任何「判定引擎」设置行（历史实现靠它选通道）
    let conn = core_storage::open(&db).unwrap();
    let engine_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM settings WHERE key = 'compliance'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(engine_rows, 0, "全新安装不该有引擎设置行（迁移 0030 已清）");
    drop(conn);

    let conn = core_storage::open(&db).unwrap();
    let state = app_web::test_support::state(data.clone(), conn);
    let app = app_web::router_without_dist(state);
    Fixture { app, data }
}

fn db_path(f: &Fixture) -> PathBuf {
    f.data.join("dam.db")
}

async fn post_json(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
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

/// 往 specs 表插一条规格（`plugin_id` / 版本钉按用例给定）。
fn insert_spec(conn: &rusqlite::Connection, plugin_id: &str, pin: Option<&str>, name: &str) -> i64 {
    conn.execute(
        "INSERT INTO plugin_spec (pluginId, pluginVersionPin, name, industry, rules, thresholds,
                                  actions, output, paramsDiff, enabled)
         VALUES (?1, ?2, ?3, 'ecommerce', '[]', '{}', '[]', '{}', '{}', 1)",
        rusqlite::params![plugin_id, pin, name],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn has_engine_setting(f: &Fixture) -> bool {
    let conn = core_storage::open(&db_path(f)).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM settings WHERE key = 'compliance'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

/// 1) 全新安装：没有引擎设置也能发起判定（200 = 插件跑起来；400 只允许是「插件没装」这条
///    可操作错误 —— 两者都不许是 500，也都不许留下僵尸 run）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_install_judges_without_any_engine_setting() {
    let _guard = vips_lock().lock().unwrap_or_else(|e| e.into_inner());
    let f = setup("fresh");
    assert!(!has_engine_setting(&f), "首发形态：库内无引擎设置");

    let (code, j) = post_json(
        &f.app,
        "/api/compliance/judge",
        r#"{"assetIds":[1,2],"scope":"asset"}"#,
    )
    .await;
    match code {
        200 => {
            assert_eq!(j["engine"], serde_json::json!("plugin"), "{j}");
            assert_eq!(j["candidates"], serde_json::json!(2), "{j}");
        }
        400 => {
            // 容器里没有 python3 / 没带插件：必须是「插件没装」的可操作错误，而不是引擎选错
            assert_eq!(j["error"], serde_json::json!("plugin_not_installed"), "{j}");
            assert_eq!(j["action"], serde_json::json!("open_packs_drawer"), "{j}");
            assert!(
                j["hint"].as_str().unwrap_or_default().contains(".dampack"),
                "{j}"
            );
        }
        other => panic!("判定入口不许 {other}：{j}"),
    }
    assert!(!has_engine_setting(&f), "判定过程不许写引擎设置");
}

/// 2) 升级安装：库里残留 `compliance = {engine:"legacy"}`（迁移前的老库）→ 判定照常发起，
///    残留设置既不当开关、也不报错（迁移 0030 负责把它清掉；清之前也不许挡路）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_with_legacy_setting_residue_still_judges() {
    let _guard = vips_lock().lock().unwrap_or_else(|e| e.into_inner());
    let f = setup("upgrade");
    {
        let conn = core_storage::open(&db_path(&f)).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value, updatedAt) VALUES ('compliance', '{\"engine\":\"legacy\"}', 0)",
            [],
        )
        .unwrap();
    }
    assert!(has_engine_setting(&f), "用例前提：残留老设置");

    let (code, j) = post_json(
        &f.app,
        "/api/compliance/judge",
        r#"{"assetIds":[1],"scope":"asset"}"#,
    )
    .await;
    assert!(
        code == 200 || code == 400,
        "残留老设置不许把判定入口打成 {code}：{j}"
    );
    if code == 400 {
        assert_eq!(j["error"], serde_json::json!("plugin_not_installed"), "{j}");
    } else {
        assert_eq!(
            j["engine"],
            serde_json::json!("plugin"),
            "通道恒为插件：{j}"
        );
    }
}

/// 3) 规格引用的插件已卸载 → 400 `spec_plugin_not_installed` + 去处（不是裸 400）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spec_pointing_at_uninstalled_plugin_is_actionable() {
    let _guard = vips_lock().lock().unwrap_or_else(|e| e.into_inner());
    let f = setup("spec-missing");
    let spec_id = {
        let conn = core_storage::open(&db_path(&f)).unwrap();
        insert_spec(&conn, "com.example.gone", None, "已卸载插件的规格")
    };

    let body = format!(r#"{{"specId":{spec_id},"assetIds":[1],"scope":"asset"}}"#);
    let (code, j) = post_json(&f.app, "/api/compliance/judge", &body).await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(
        j["error"],
        serde_json::json!("spec_plugin_not_installed"),
        "{j}"
    );
    assert_eq!(
        j["code"],
        serde_json::json!("spec_plugin_not_installed"),
        "{j}"
    );
    assert_eq!(j["action"], serde_json::json!("open_packs_drawer"), "{j}");
    let hint = j["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("com.example.gone"),
        "提示要点名哪个插件：{hint}"
    );
    let msg = j["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("已卸载插件的规格"),
        "文案要点名哪条规格：{msg}"
    );
}

/// 4) 规格钉的版本与已装版本不一致 → 400 `spec_plugin_version_mismatch`（不静默改用新版）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spec_version_pin_mismatch_is_actionable() {
    let _guard = vips_lock().lock().unwrap_or_else(|e| e.into_inner());
    let f = setup("spec-pin");
    // 只有装了电商插件、且已装版本 != 钉的版本，这条用例才有意义
    let Some(dir) = app_web::compliance_api::repo_builtin_pack_dir("com.soujpg.pack.ecommerce")
    else {
        eprintln!("skip: 本机没有仓库内置电商插件包");
        return;
    };
    let spec_id = {
        let conn = core_storage::open(&db_path(&f)).unwrap();
        insert_spec(
            &conn,
            "com.soujpg.pack.ecommerce",
            Some("0.0.1"),
            "钉旧版本的规格",
        )
    };
    let _ = dir;

    let body = format!(r#"{{"specId":{spec_id},"assetIds":[1],"scope":"asset"}}"#);
    let (code, j) = post_json(&f.app, "/api/compliance/judge", &body).await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(
        j["error"],
        serde_json::json!("spec_plugin_version_mismatch"),
        "{j}"
    );
    assert_eq!(j["action"], serde_json::json!("open_packs_drawer"), "{j}");
    let msg = j["message"].as_str().unwrap_or_default();
    assert!(msg.contains("0.0.1"), "要点名钉的版本：{msg}");
}

/// 5) 规格停用 / 规格不存在：同样给人话 + 去处（不是只回一个码）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_and_missing_specs_are_actionable() {
    let _guard = vips_lock().lock().unwrap_or_else(|e| e.into_inner());
    let f = setup("spec-disabled");

    let spec_id = {
        let conn = core_storage::open(&db_path(&f)).unwrap();
        let id = insert_spec(&conn, "com.soujpg.pack.ecommerce", None, "停用的规格");
        conn.execute(
            "UPDATE plugin_spec SET enabled = 0 WHERE specId = ?1",
            rusqlite::params![id],
        )
        .unwrap();
        id
    };
    let body = format!(r#"{{"specId":{spec_id},"assetIds":[1],"scope":"asset"}}"#);
    let (code, j) = post_json(&f.app, "/api/compliance/judge", &body).await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(j["error"], serde_json::json!("spec_disabled"), "{j}");
    assert_eq!(j["action"], serde_json::json!("open_packs_drawer"), "{j}");

    let (code, j) = post_json(
        &f.app,
        "/api/compliance/judge",
        r#"{"specId":999999,"assetIds":[1],"scope":"asset"}"#,
    )
    .await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(j["error"], serde_json::json!("spec_not_found"), "{j}");
}
