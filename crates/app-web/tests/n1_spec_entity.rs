#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! N1 规格实体（卡 task）HTTP 面回归钉：**规格 = 配置，不安装不分发**。
//!
//! 钉的六件事：
//! 1. 老库首次读到 `/api/specs` 会惰性补三条默认规格（淘宝主图 / 亚马逊主图 / 房源照片）；
//! 2. `POST /api/specs/migrate` 幂等（第二遍不新建）+ 回读校验通过 + 阈值与旧 YAML 逐值一致；
//! 3. CRUD：建/列/取/改/删 + 同插件重名 409 + 未知规则 400 + 老包不许存 paramsDiff 400；
//! 4. **插件不在 / 版本不匹配时规格仍可见**（不静默消失）；
//! 5. `judge` 收 `specId`：生效参数快照里带 `spec` 段、job payload 带规则清单；
//! 6. 停用的规格不许被引用（400 `spec_disabled`）；引擎档位不匹配要 400（不许静默忽略参数）。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tower::ServiceExt;

fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

/// 取锁时容忍中毒（别的用例 panic 不该把本用例变二次 panic，掩盖真实失败）
fn vips_guard() -> std::sync::MutexGuard<'static, ()> {
    vips_lock().lock().unwrap_or_else(|e| e.into_inner())
}

fn make_png(path: &Path, brighten: f64) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    call(app, "GET", uri, None).await
}

async fn post(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    call(app, "POST", uri, Some(body)).await
}

async fn patch(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    call(app, "PATCH", uri, Some(body)).await
}

async fn delete(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    call(app, "DELETE", uri, None).await
}

/// 独立 data_dir + 库（每个用例一套，互不干扰）。
fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("n1-spec-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    drop(conn);
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

/// 两张图直接落库（与 CMP-JOB-01 的夹具同口径：本卡不测导入链路）。
fn seed_assets(dir: &Path, root: &Path) -> Vec<i64> {
    let conn = core_storage::open(&db_path(dir)).unwrap();
    std::fs::create_dir_all(root).unwrap();
    let mut ids = Vec::new();
    for k in 0..2 {
        let p = root.join(format!("img{k}.png"));
        make_png(&p, 40.0 + k as f64 * 30.0);
        let id = (k + 1) as i64;
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'png', 10, 0, ?1)",
            rusqlite::params![
                id,
                p.to_string_lossy().to_string(),
                p.file_name().unwrap().to_string_lossy().to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 64, 64)",
            [id],
        )
        .unwrap();
        ids.push(id);
    }
    ids
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("dam.db")
}

/// 一条自定义规格的 body（电商插件 + 只判两条规则 + 改阈值）
fn spec_body(name: &str) -> String {
    serde_json::json!({
        "pluginId": "com.soujpg.pack.ecommerce",
        "name": name,
        "industry": "ecommerce",
        "rules": ["R1_size", "R2_white_bg"],
        "thresholds": {"R1_size": {"min": 800}},
        "paramsDiff": {"shortSideMin": 800},
        "actions": [{"action": "resize", "params": {"maxWidth": 1200}}],
        "output": {"format": "webp"}
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// 1+2：默认规格 / 迁移幂等 / 阈值保真
// ---------------------------------------------------------------------------

/// 检查类默认规格条数（淘宝主图 / 亚马逊主图 / 房源照片）
const CHECK_SEEDS: usize = 3;

/// 期望的**处理类官方默认方案**：仓库内置的纯处理包（`kinds` 含 process、不含 judge）
/// 且 `resources/actions.json` **恰好声明一个动作** → 宿主派生一条方案（`seed:<pack id>`，
/// 方案名 = 包内动作名，动作链 = 包内那个唯一动作）。
///
/// 为什么不写死条数/名单：口径是「一个动作的包派生一条」，KI-39 只有段包，KI-40 加上
/// upscale 包就多一条 —— 写死 `total == 4` 会让**合规的新包**变红（正是本卡撞到的）。
/// 这里独立读包目录，不调产品代码，避免「拿实现验实现」。
fn expected_process_seeds() -> Vec<(String, String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/plugins");
    let mut out: Vec<(String, String, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(raw) = std::fs::read_to_string(dir.join("manifest.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let kinds: Vec<String> = manifest
            .get("kinds")
            .and_then(|k| k.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !kinds.iter().any(|k| k == "process") || kinds.iter().any(|k| k == "judge") {
            continue;
        }
        let Ok(raw_actions) = std::fs::read_to_string(dir.join("resources").join("actions.json"))
        else {
            continue;
        };
        let Ok(actions) = serde_json::from_str::<serde_json::Value>(&raw_actions) else {
            continue;
        };
        let Some(list) = actions.get("actions").and_then(|a| a.as_array()) else {
            continue;
        };
        if list.len() != 1 {
            continue; // 多动作包派生哪一条都是猜 → 宿主不派生
        }
        let Some(pack_id) = manifest.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(action_id) = list[0].get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = list[0]
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| pack_id.to_string());
        out.push((pack_id.to_string(), name, action_id.to_string()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0)); // 落库顺序 = plugin_id 升序
    out
}

#[tokio::test]
async fn list_lazily_seeds_default_specs() {
    let (_dir, app) = fresh_state("seed");
    let (code, j) = get(&app, "/api/specs").await;
    assert_eq!(code, 200, "{j}");
    // 3 条检查类（淘宝/Amazon/房源）+ 每个「恰好声明一个动作」的处理包一条官方默认方案
    // （KI-39 / task 定了口径，KI-40 的 upscale 包按同一口径自动多一条）
    let expect_process = expected_process_seeds();
    assert!(
        !expect_process.is_empty(),
        "repo 内置处理包应有可派生的默认方案"
    );
    assert_eq!(
        j["total"].as_u64().unwrap() as usize,
        CHECK_SEEDS + expect_process.len(),
        "默认规格 = {CHECK_SEEDS} 检查 + {} 处理（{:?}）：{j}",
        expect_process.len(),
        expect_process
    );
    let names: Vec<&str> = j["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        &names[..CHECK_SEEDS],
        &["淘宝主图", "亚马逊主图", "房源照片"],
        "{names:?}"
    );
    let mut process_names: Vec<String> =
        names[CHECK_SEEDS..].iter().map(|s| s.to_string()).collect();
    process_names.sort();
    let mut want: Vec<String> = expect_process.iter().map(|(_, n, _)| n.clone()).collect();
    want.sort();
    assert_eq!(process_names, want, "处理类方案名 = 包内动作名：{names:?}");
    // 每条都带现算的 pluginStatus（插件在不在是环境事实，不是规格属性）
    for item in j["items"].as_array().unwrap() {
        assert!(item["pluginStatus"]["installed"].is_boolean(), "{item}");
        assert!(
            item["pluginStatus"]["versionMismatch"].is_boolean(),
            "{item}"
        );
    }
    // 淘宝规格的阈值来自插件 resources/rules.json（IP-K16 起旧 YAML 已删，包资源是唯一权威）
    let taobao = j["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "淘宝主图")
        .unwrap();
    assert_eq!(taobao["thresholds"]["R1_size"]["short_side_min"], 1600);
    assert_eq!(taobao["thresholds"]["R2_white_bg"]["max_dev"], 8);
    assert_eq!(taobao["paramsDiff"]["shortSideMin"], 1600);
    assert_eq!(taobao["originKey"], "legacy-pack:ecommerce-taobao");
    // 处理类官方默认方案：动作链是包内声明的那个唯一动作（不是宿主写死的字面量），
    // 且它没有规则/阈值（处理方案的内容就是动作链）
    let segment = j["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["originKey"] == "seed:com.soujpg.pack.segment")
        .expect("段包必须有官方默认方案");
    assert_eq!(
        segment["actions"],
        serde_json::json!([{"action": "white_bg_fill"}]),
        "{segment}"
    );
    assert_eq!(segment["rules"], serde_json::json!([]), "{segment}");
    assert_eq!(segment["pluginId"], "com.soujpg.pack.segment", "{segment}");
    // 每条处理类方案都按包内声明逐项对账（upscale 包 KI-40 起走同一口径，不是特例）
    for (pack_id, name, action_id) in &expect_process {
        let seed = j["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["originKey"].as_str() == Some(format!("seed:{pack_id}").as_str()))
            .unwrap_or_else(|| panic!("{pack_id} 必须有官方默认方案：{j}"));
        assert_eq!(seed["name"].as_str(), Some(name.as_str()), "{seed}");
        assert_eq!(seed["pluginId"].as_str(), Some(pack_id.as_str()), "{seed}");
        assert_eq!(
            seed["actions"],
            serde_json::json!([{ "action": action_id.as_str() }]),
            "{seed}"
        );
        assert_eq!(seed["rules"], serde_json::json!([]), "{seed}");
    }
}

#[tokio::test]
async fn migrate_is_idempotent_and_reports_readback() {
    let (_dir, app) = fresh_state("migrate");
    let (code, first) = post(&app, "/api/specs/migrate", "{}").await;
    assert_eq!(code, 200, "{first}");
    let want_seeds = CHECK_SEEDS + expected_process_seeds().len();
    assert_eq!(
        first["created"].as_array().unwrap().len(),
        want_seeds,
        "{first}"
    );
    assert_eq!(first["verification"]["ok"], true, "{first}");
    assert_eq!(first["verification"]["originUnique"], true);
    assert_eq!(first["verification"]["thresholdsMatchSource"], true);

    // 第二遍：一条也不新建（幂等），回读仍通过
    let (code, second) = post(&app, "/api/specs/migrate", "{}").await;
    assert_eq!(code, 200, "{second}");
    assert_eq!(
        second["created"].as_array().unwrap().len(),
        0,
        "迁移必须幂等：{second}"
    );
    assert_eq!(second["existing"].as_array().unwrap().len(), want_seeds);
    assert_eq!(second["verification"]["ok"], true, "{second}");
    println!(
        "[N1] migrate(repeat) = {}",
        serde_json::to_string_pretty(&second).unwrap()
    );

    // 房源规格：旧 YAML 的三个数值阈值都在（禁止静默丢）
    let realestate = second["existing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["originKey"] == "legacy-pack:realestate")
        .expect("房源规格必须在");
    let _ = realestate; // 阈值明细在 GET 侧断言（existing 段只回摘要字段）

    let (code, j) = get(&app, "/api/specs?plugin=com.soujpg.pack.realestate").await;
    assert_eq!(code, 200, "{j}");
    let re = &j["items"][0];
    assert_eq!(re["thresholds"]["F1_sharpness"]["min"], 100.0, "{re}");
    assert_eq!(
        re["thresholds"]["F2_exposure"]["max_dead_ratio"], 0.3,
        "{re}"
    );
    assert_eq!(re["thresholds"]["F3_duplicate"]["threshold"], 8, "{re}");
    assert_eq!(
        re["paramsDiff"],
        serde_json::json!({}),
        "老包没有 schema，不许造 paramsDiff"
    );
}

/// 默认规格是**一次性迁移**：用户把三条默认规格删光后，列表不许把它们变回来
/// （否则「删除」等于没发生过）。
#[tokio::test]
async fn deleting_default_specs_does_not_resurrect_them() {
    let (_dir, app) = fresh_state("no-resurrect");
    let (code, j) = get(&app, "/api/specs").await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(
        j["total"].as_u64().unwrap() as usize,
        CHECK_SEEDS + expected_process_seeds().len(),
        "首次访问补默认规格（{CHECK_SEEDS} 检查 + 各单动作处理包一条）：{j}"
    );
    for item in j["items"].as_array().unwrap() {
        let id = item["specId"].as_i64().unwrap();
        let (code, gone) = delete(&app, &format!("/api/specs/{id}")).await;
        assert_eq!(code, 200, "{gone}");
    }
    let (code, after) = get(&app, "/api/specs").await;
    assert_eq!(code, 200, "{after}");
    assert_eq!(after["total"], 0, "删光后不许复活默认规格：{after}");
}

// ---------------------------------------------------------------------------
// 3：CRUD + 校验错误码
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crud_roundtrip_and_validation_errors() {
    let (_dir, app) = fresh_state("crud");
    // 建
    let (code, created) = post(&app, "/api/specs", &spec_body("我的 800px webp")).await;
    assert_eq!(code, 201, "{created}");
    let id = created["specId"].as_i64().expect("必须回 specId");
    assert_eq!(
        created["rules"],
        serde_json::json!(["R1_size", "R2_white_bg"])
    );
    assert_eq!(created["paramsDiff"]["shortSideMin"], 800);
    assert_eq!(
        created["originKey"],
        serde_json::Value::Null,
        "用户自建没有迁移来源"
    );

    // 列（按插件过滤）
    let (code, j) = get(&app, "/api/specs?plugin=com.soujpg.pack.ecommerce").await;
    assert_eq!(code, 200, "{j}");
    assert!(
        j["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["specId"] == id),
        "自建规格必须能列出来：{j}"
    );

    // 取
    let (code, one) = get(&app, &format!("/api/specs/{id}")).await;
    assert_eq!(code, 200, "{one}");
    assert_eq!(one["name"], "我的 800px webp");

    // 改（只给一个字段：其余按现值保留）
    let (code, updated) = patch(
        &app,
        &format!("/api/specs/{id}"),
        r#"{"name":"我的 800px webp v2"}"#,
    )
    .await;
    assert_eq!(code, 200, "{updated}");
    assert_eq!(updated["name"], "我的 800px webp v2");
    assert_eq!(
        updated["paramsDiff"]["shortSideMin"], 800,
        "没给的字段不许被清空"
    );

    // 同插件重名 → 409
    let (code, dup) = post(&app, "/api/specs", &spec_body("我的 800px webp v2")).await;
    assert_eq!(code, 409, "同插件重名必须 409：{dup}");
    assert_eq!(dup["error"], "spec_duplicate_name");

    // 未知规则 → 400
    let (code, bad_rule) = post(
        &app,
        "/api/specs",
        r#"{"pluginId":"com.soujpg.pack.ecommerce","name":"坏规格","rules":["R9_nope"]}"#,
    )
    .await;
    assert_eq!(code, 400, "{bad_rule}");
    assert_eq!(bad_rule["error"], "spec_unknown_rule", "{bad_rule}");

    // 参数越界 → 400（错误码沿用 K07-I 的稳定集合）
    let (code, bad_param) = post(&app, "/api/specs", r#"{"pluginId":"com.soujpg.pack.ecommerce","name":"越界","paramsDiff":{"shortSideMin":10}}"#).await;
    assert_eq!(code, 400, "{bad_param}");
    assert_eq!(bad_param["error"], "params_range", "{bad_param}");

    // 老包（无 schema）不许存 paramsDiff → 400
    let (code, no_schema) = post(&app, "/api/specs", r#"{"pluginId":"com.soujpg.pack.realestate","name":"房产自定义","paramsDiff":{"whatever":1}}"#).await;
    assert_eq!(code, 400, "{no_schema}");
    assert_eq!(no_schema["error"], "spec_params_unsupported", "{no_schema}");

    // 阈值必须是数值
    let (code, bad_th) = post(&app, "/api/specs", r#"{"pluginId":"com.soujpg.pack.ecommerce","name":"坏阈值","thresholds":{"R1_size":{"min":"高"}}}"#).await;
    assert_eq!(code, 400, "{bad_th}");
    assert_eq!(bad_th["error"], "spec_invalid_threshold");

    // 删
    let (code, gone) = delete(&app, &format!("/api/specs/{id}")).await;
    assert_eq!(code, 200, "{gone}");
    assert_eq!(gone["deleted"], true);
    let (code, missing) = get(&app, &format!("/api/specs/{id}")).await;
    assert_eq!(code, 404, "{missing}");
    assert_eq!(missing["error"], "spec_not_found");
}

// ---------------------------------------------------------------------------
// 4：插件卸载/升级后规格仍可见
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spec_stays_visible_when_plugin_missing_or_version_drifted() {
    let (_dir, app) = fresh_state("uninstalled");
    // 一个「装过又卸了」的插件身份：仓内没有任何包叫这个名字
    let (code, created) = post(
        &app,
        "/api/specs",
        r#"{"pluginId":"com.acme.pack.retouch","name":"我的抠图规格","rules":[],"thresholds":{"X":{"min":1}}}"#,
    )
    .await;
    assert_eq!(code, 201, "插件不在也必须能建/存规格：{created}");
    let id = created["specId"].as_i64().unwrap();
    assert_eq!(created["pluginStatus"]["installed"], false, "{created}");
    assert!(
        created["pluginStatus"]["note"]
            .as_str()
            .unwrap()
            // DM-B1（task）改了这句人话：不再拼裸 id、把「规格」换成用户面名词「方案」。
            // 断言改成钉「说清为什么不可执行」这件事本身，而不是某一句文案。
            .contains("还没装"),
        "必须说清为什么不可执行：{created}"
    );

    // 卸载插件后再读：规格行还在（不静默消失）
    let (code, j) = get(&app, "/api/specs").await;
    assert_eq!(code, 200, "{j}");
    let row = j["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["specId"] == id)
        .expect("插件不在时规格必须仍然列得出来（用户资产不许被吃掉）");
    assert_eq!(row["pluginStatus"]["installed"], false);
    assert_eq!(row["thresholds"]["X"]["min"], 1, "阈值原样保留");

    // 版本不匹配：钉了版本而当前版本不同 → versionMismatch=true，但行仍可见可改
    let (code, pinned) = post(
        &app,
        "/api/specs",
        r#"{"pluginId":"com.soujpg.pack.ecommerce","name":"钉死 1.0.0 的规格","pluginVersionPin":"1.0.0"}"#,
    )
    .await;
    assert_eq!(code, 201, "{pinned}");
    assert_eq!(pinned["pluginStatus"]["installed"], true, "{pinned}");
    assert_eq!(
        pinned["pluginStatus"]["versionMismatch"], true,
        "钉的版本与已装版本不同必须标出来：{pinned}"
    );
    let pid = pinned["specId"].as_i64().unwrap();
    let (code, still) = patch(&app, &format!("/api/specs/{pid}"), r#"{"enabled":false}"#).await;
    assert_eq!(code, 200, "版本不匹配的规格仍然可编辑：{still}");
    assert_eq!(still["enabled"], false);
}

// ---------------------------------------------------------------------------
// 5+6：judge 引用规格（行为证据）
// ---------------------------------------------------------------------------

/// 仓库内置的合规插件（`packs/plugins/ecommerce`）的规则清单 —— IP-K16 之后
/// 「能跑判定的东西」只有代码插件，没有可导入的 YAML 行业包了，所以规格直接挂在它上面。
/// 「规则 = 包内全部」这条口径照旧成立（子集留给用户在界面上勾，这里钉的是全量能跑通）。
const ECOMMERCE_RULES: [&str; 6] = [
    "R0_product_main",
    "R1_size",
    "R2_white_bg",
    "R4_subject_ratio",
    "R3_no_watermark",
    "R5_attribute",
];

fn ecommerce_rules_json() -> serde_json::Value {
    serde_json::json!(ECOMMERCE_RULES)
}

#[tokio::test]
async fn judge_with_spec_id_snapshots_spec_and_rules() {
    let _vips = vips_guard();
    let (dir, app) = fresh_state("judge");
    // 资产直接落库（本卡不测导入链路；判定管道只认库里已有的资产）
    seed_assets(&dir, &dir.join("src"));

    // 规格：挂在仓库内置的电商插件上（KI-31 起按规格的 pluginId 开包，不需要「激活」动作）
    let (code, spec) = post(
        &app,
        "/api/specs",
        &serde_json::json!({
            "pluginId": "com.soujpg.pack.ecommerce",
            "name": "房源初审",
            "rules": ECOMMERCE_RULES,
            "thresholds": {"R1_size": {"short_side_min": 2}},
        })
        .to_string(),
    )
    .await;
    assert_eq!(code, 201, "{spec}");
    let spec_id = spec["specId"].as_i64().unwrap();

    let (code, start) = post(
        &app,
        "/api/compliance/judge",
        &serde_json::json!({ "specId": spec_id, "all": true, "scope": "asset" }).to_string(),
    )
    .await;
    assert_eq!(code, 200, "带 specId 的 judge 必须入队成功：{start}");
    let run_id = start["runId"].as_i64().expect("必须回 runId");

    // 生效参数快照落 run.params：必须带 spec 段（重跑能说清「用哪条规格」）
    let conn = core_storage::open(&db_path(&dir)).unwrap();
    let params: String = conn
        .query_row(
            "SELECT params FROM compliance_runs WHERE runId = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .unwrap();
    let params: serde_json::Value = serde_json::from_str(&params).unwrap();
    assert_eq!(params["spec"]["specId"], spec_id, "{params}");
    assert_eq!(params["spec"]["name"], "房源初审");
    assert_eq!(
        params["spec"]["thresholds"]["R1_size"]["short_side_min"], 2,
        "阈值必须进快照：{params}"
    );
    assert_eq!(params["spec"]["rules"], ecommerce_rules_json());
    println!(
        "[N1] run{run_id} params = {}",
        serde_json::to_string_pretty(&params).unwrap()
    );

    // job payload 带上规格的规则清单（「判哪些规则」跟着任务走）
    let payload: String = conn
        .query_row(
            "SELECT payload FROM job_queue WHERE kind = 'compliance' AND runId = ?1 LIMIT 1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        payload["rules"],
        ecommerce_rules_json(),
        "job payload 必须带规格的规则清单：{payload}"
    );
    println!("[N1] job payload = {payload}");

    // 停用的规格不许被引用
    let (code, _) = patch(
        &app,
        &format!("/api/specs/{spec_id}"),
        r#"{"enabled":false}"#,
    )
    .await;
    assert_eq!(code, 200);
    let (code, disabled) = post(
        &app,
        "/api/compliance/judge",
        &serde_json::json!({ "specId": spec_id, "all": true, "scope": "asset" }).to_string(),
    )
    .await;
    assert_eq!(code, 400, "停用规格必须拒绝：{disabled}");
    assert_eq!(disabled["error"], "spec_disabled", "{disabled}");

    // 未知规则：建规格时就必须拒（不许静默忽略参数）—— 插件规则清单是唯一权威
    let (code, unknown) = post(
        &app,
        "/api/specs",
        r#"{"pluginId":"com.soujpg.pack.ecommerce","name":"坏规格","rules":["NOPE_rule"]}"#,
    )
    .await;
    assert_eq!(code, 400, "未知规则必须 400：{unknown}");
    assert_eq!(unknown["error"], "spec_unknown_rule", "{unknown}");

    // 不存在的规格
    let (code, nope) = post(
        &app,
        "/api/compliance/judge",
        r#"{"specId":99999,"all":true,"scope":"asset"}"#,
    )
    .await;
    assert_eq!(code, 400, "{nope}");
    assert_eq!(nope["error"], "spec_not_found");
}

/// 「先跑 1 张」：specs/{id}/try 走同一套 judge 路径（不是第二条执行链路）。
#[tokio::test]
async fn try_one_runs_a_single_asset_via_judge() {
    let _vips = vips_guard();
    let (dir, app) = fresh_state("try");
    seed_assets(&dir, &dir.join("src"));
    let (code, spec) = post(
        &app,
        "/api/specs",
        &serde_json::json!({
            "pluginId": "com.soujpg.pack.ecommerce",
            "name": "试跑规格",
            "rules": ECOMMERCE_RULES,
        })
        .to_string(),
    )
    .await;
    assert_eq!(code, 201, "{spec}");
    let spec_id = spec["specId"].as_i64().unwrap();

    let (code, out) = post(&app, &format!("/api/specs/{spec_id}/try"), "{}").await;
    assert_eq!(code, 200, "先跑 1 张必须成功：{out}");
    assert_eq!(out["candidates"], 1, "试跑只跑一张：{out}");
    let run_id = out["runId"].as_i64().unwrap();
    let conn = core_storage::open(&db_path(&dir)).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind = 'compliance' AND runId = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "试跑只入队 1 个资产 job");
}

/// 设计口径里的入口名 `POST /api/jobs` 也要能吃 `specId`（直通 judge，不是第二条执行链路）。
///
/// 这里用「不存在的规格」来证明请求确实进了 judge 的规格校验（而不是落到别处）：
/// 既证明路由在、也证明 body 的 `specId` 被解析（不需要真的跑一次判定，避免与
/// 「已有判定在跑」的运行槽冲突）。
#[tokio::test]
async fn post_jobs_accepts_spec_id_and_reaches_judge() {
    let (_dir, app) = fresh_state("jobs-alias");
    let (code, j) = post(
        &app,
        "/api/jobs",
        r#"{"specId":99999,"all":true,"scope":"asset"}"#,
    )
    .await;
    assert_eq!(
        code, 400,
        "POST /api/jobs 必须解析 specId 并交给 judge 校验：{j}"
    );
    assert_eq!(j["error"], "spec_not_found", "{j}");
    // 路由确实挂在 /api 下（不是 /api/compliance/jobs）
    let (code, _) = post(&app, "/api/compliance/jobs", "{}").await;
    assert_eq!(
        code, 405,
        "POST /api/compliance/jobs 仍只有 GET（不新增第二个建任务入口）"
    );
}

// ---------------------------------------------------------------------------
// 4b：真的卸载插件（.dampack 装 → 卸）也不删规格
// ---------------------------------------------------------------------------

/// 现场打一个最小 `.dampack`（只验安装/卸载对规格的影响，不启子进程）。
fn demo_pack(fixtures: &Path, id: &str) -> PathBuf {
    use std::io::Write as _;
    let manifest = format!(
        r#"{{
  "id": "{id}",
  "name": "N1 夹具包",
  "version": "1.0.0",
  "api_version": "1.1",
  "entrypoint": {{"cmd": "python3", "args": ["plugin/__init__.py"]}},
  "capabilities": ["io.read", "op.geometric", "report.emit"],
  "deterministic": true,
  "timeout_secs": 30,
  "resources": ["resources/rules.json"],
  "permissions": {{"read": true, "net": false}},
  "signature": null
}}"#
    );
    let entries = vec![
        ("manifest.json".to_string(), manifest.into_bytes()),
        (
            "plugin/__init__.py".to_string(),
            b"def serve():\n    pass\n".to_vec(),
        ),
        (
            "resources/rules.json".to_string(),
            r#"{"rules":[{"id":"C1_bg","name":"white"},{"id":"C2_size","name":"size"}]}"#
                .as_bytes()
                .to_vec(),
        ),
    ];
    std::fs::create_dir_all(fixtures).unwrap();
    let path = fixtures.join("n1-demo.dampack");
    let mut f = std::fs::File::create(&path).unwrap();
    pack_host::write_archive(&mut f, &entries).unwrap();
    f.flush().unwrap();
    path
}

#[tokio::test]
async fn uninstalling_plugin_keeps_spec_rows() {
    let (dir, app) = fresh_state("uninstall-e2e");
    let id = "com.example.n1.retouch";
    let pack = demo_pack(&dir.join("fixtures"), id);
    let (code, installed) = post(
        &app,
        "/api/compliance/packs/install",
        &serde_json::json!({ "path": pack.to_string_lossy() }).to_string(),
    )
    .await;
    assert_eq!(code, 200, "装包必须成功：{installed}");

    // 装好后建规格（规则名按包声明校验）
    let (code, spec) = post(
        &app,
        "/api/specs",
        &serde_json::json!({
            "pluginId": id,
            "name": "我的抠图规格",
            "rules": ["C1_bg"],
            "thresholds": {"C1_bg": {"max_dev": 6}}
        })
        .to_string(),
    )
    .await;
    assert_eq!(code, 201, "{spec}");
    let spec_id = spec["specId"].as_i64().unwrap();
    assert_eq!(spec["pluginStatus"]["installed"], true, "{spec}");
    assert_eq!(spec["pluginStatus"]["version"], "1.0.0");

    // 卸载插件（当前版本是 current，先卸 → 接口会拒绝？那就显式卸载该版本）
    let (code, gone) = delete(&app, &format!("/api/compliance/packs/{id}")).await;
    assert!(code == 200 || code == 409, "卸载接口返回：{code} {gone}");
    if code == 409 {
        // current 版本不许直接卸：先停用（用 rollback/activate 换到别的版本才有意义）；
        // 这里改验「账本仍保留该版本」的口径，规格不受影响
        assert_eq!(gone["error"], "pack_version_current", "{gone}");
    }

    // 关键断言：规格行还在（阈值原样），且不再声称可执行
    let (code, j) = get(&app, &format!("/api/specs/{spec_id}")).await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["name"], "我的抠图规格");
    assert_eq!(j["thresholds"]["C1_bg"]["max_dev"], 6, "阈值原样保留：{j}");
    assert_eq!(
        j["pluginStatus"]["installed"], false,
        "插件已卸载 → 状态如实标 false，但规格不许消失：{j}"
    );
}
