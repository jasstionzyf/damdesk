//! IP-K16（卡 task 复审整改）回归钉：**插件元数据装载失败必须到人**。
//!
//! 复审实测缺陷：`plugin_meta_pack` 用 `if let Ok(p) = Pack::from_plugin_dir(..)` 把装载错误
//! 咽掉，于是给 `resources/rules.json` 塞一个宿主不解释的键（`severity`）后：
//! * `GET /api/compliance/status` → `pluginState.available=true` + `pack.rules=0` + `note=null`
//!   （看起来「插件好用、只是还没规则」）；
//! * `POST /api/compliance/judge` → **200 + candidates=0**（用户以为审过了，实际一条规则都没读）。
//!
//! 「配了不读一律装载期拒绝」这条守卫在 `core_config::Pack::from_plugin_dir` 里是硬拒的，
//! 缺陷只在**宿主把这层错误吞掉**。本文件钉死整改后的口径：
//! 1. 坏键 → `pluginState{available:false, reason:plugin_meta_invalid}`，`note`/`packError` 点名坏键；
//! 2. 坏键 → `POST /judge` 显式 4xx（不是 200 + 0 候选），且**不产生 run 行**；
//! 3. 对照：好包仍然 `available:true` + 规则条数 > 0 + `packError:null`（整改没把正常路径打死）。
//!
//! 用法与 `judge_entry_routes.rs` 一致：临时数据目录 + 真 Router（`router_without_dist`）。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

const PLUGIN_ID: &str = "com.soujpg.pack.ecommerce";

/// 仓库自带的电商插件包（开发/容器形态的唯一事实源）。
fn repo_pack() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packs")
        .join("plugins")
        .join("ecommerce")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// 把电商插件包安装到 `{data}/packs/<id>/`（三级解析的第 3 级，优先于仓库内置路径），
/// `break_key` 给了就往第 1 条规则里加一个**宿主不解释、也没声明为插件私有**的键。
fn install_pack(data: &Path, break_key: Option<&str>) {
    let dst = data.join("packs").join(PLUGIN_ID);
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&repo_pack(), &dst);
    if let Some(key) = break_key {
        let p = dst.join("resources").join("rules.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        // 老 YAML 时代很常见的写法（`evidence`/`check`/`group_by` 同理）——
        // 宿主按设计不读它，插件也不读它，属于「配了不读」必须响亮失败的那类。
        v["rules"][0][key] = serde_json::json!("should_not_be_silently_ignored");
        std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}

struct Fixture {
    app: axum::Router,
    #[allow(dead_code)]
    data: PathBuf,
}

/// 装配：临时数据目录 + 迁移好的库 + 已安装（可能坏的）插件包。
///
/// 本文件是独立测试二进制（独立进程），所以这里清 `DAM_PACK_DIR` 是安全的：
/// 必须让三级解析命中我们自己装的包，而不是外层环境变量指定的目录。
fn setup(tag: &str, break_key: Option<&str>) -> Fixture {
    std::env::remove_var("DAM_PACK_DIR");
    let data = std::env::temp_dir().join(format!("ipk16-meta-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    install_pack(&data, break_key);
    let conn = core_storage::open(&data.join("dam.db")).unwrap();
    let state = app_web::test_support::state(data.clone(), conn);
    let app = app_web::router_without_dist(state);
    Fixture { app, data }
}

async fn get_json(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
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
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

/// 1) 未解释键 → `/status` 报「不可用 + 点名坏键」，而不是 available + 0 规则。
#[tokio::test]
async fn unknown_rule_key_is_a_visible_error_not_available_zero_rules() {
    let f = setup("status-bad", Some("severity"));
    let (code, body) = get_json(&f.app, "/api/compliance/status").await;
    assert_eq!(code, 200, "状态接口本身不该 500（坏包要说清原因）: {body}");
    let ps = &body["pluginState"];
    assert_eq!(
        ps["available"], false,
        "坏包必须报不可用，不许 available=true: {ps}"
    );
    assert_eq!(ps["reason"], "plugin_meta_invalid", "{ps}");
    let note = ps["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("severity"),
        "note 要点名坏键（否则用户不知道改哪）: {note}"
    );
    // 顶层 packError 同源透出（前端报错条直接用，不必解析 pluginState）
    let pack_error = body["packError"].as_str().unwrap_or_default();
    assert!(pack_error.contains("severity"), "packError 要点名坏键: {body}");
    // 直白钉死复审那条假通过：绝不能「可用 + 0 条规则 + 没话说」
    assert!(
        !(ps["available"] == true && body["pack"]["rules"] == 0 && ps["note"].is_null()),
        "复审缺陷复现：available + 0 规则 + note=null: {body}"
    );
}

/// 2) 未解释键 → `/judge` 显式拒绝（不是 200 + candidates=0），且不落 run 行。
#[tokio::test]
async fn judge_refuses_loudly_when_pack_meta_is_broken() {
    let f = setup("judge-bad", Some("severity"));
    let (code, body) = post_json(&f.app, "/api/compliance/judge", r#"{"all":true}"#).await;
    assert!(
        (400..500).contains(&code),
        "元数据坏掉时 /judge 必须 4xx，实测 {code}: {body}"
    );
    assert_eq!(body["error"], "judge_prepare", "{body}");
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("severity"), "错误信息要点名坏键: {msg}");
    // 不能留下「跑了但 0 张」的空 run —— 一次执行一行，用户按行看统计
    let (jobs_code, jobs) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(jobs_code, 200);
    assert_eq!(jobs["total"], 0, "拒绝时不许开 run 行: {jobs}");
}

/// 3) 对照：好包照常可用（整改没把正常路径打死）。
#[tokio::test]
async fn good_pack_stays_available_with_rules() {
    let f = setup("status-good", None);
    let (code, body) = get_json(&f.app, "/api/compliance/status").await;
    assert_eq!(code, 200);
    let ps = &body["pluginState"];
    assert_eq!(ps["available"], true, "{ps}");
    assert!(ps["note"].is_null(), "{ps}");
    assert!(
        body["pack"]["rules"].as_u64().unwrap_or(0) > 0,
        "好包必须读到规则清单: {body}"
    );
    assert!(body["packError"].is_null(), "好包不该有 packError: {body}");
}
