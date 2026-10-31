//! DM-B1（task / KI-36）契约测试：**能力名随包下发**，界面不再维护 id → 名字 映射表。
//!
//! 卡面验收第 4 条：「`GET /api/compliance/packs` 响应每行都有非空 name（Rust 单测断言）」。
//!
//! 走真 `app_web::router` + 真 SQLite + 真 `PackStore`（与 `task_imaging_pack_routes.rs`
//! 同一套 harness），只把子进程执行换掉 —— 这些路由都不启插件进程。
//!
//! 为什么要装一个**仓库里没有的 id**（`com.soujpg.pack.imaging_demo`）：KI-36 的现象就是
//! 「装映射外的能力时 NAV/能力面板显示裸 id」—— 前端映射表只覆 5 个官方 id，只有真装一个
//! 映射外能力才能证明名字确实来自接口（manifest `short_name`），不是前端硬编码。
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

/// 映射外能力的 id（复制 `packs/plugins/upscale`，只改 manifest id）。
///
/// 为什么挑 upscale：它声明的能力（`io.read` / `op.geometric` / `report.emit`）在**默认**
/// 宿主能力集里 —— web 容器构建也能装上（imaging/segment/ecommerce 声明了 `detect.u2net`，
/// 默认构建按设计不带本地推理，会被正确拒装）。这样这条契约测试在 L0（无 `local-infer`）
/// 也能跑，不用挂 `required-features`。
const DEMO_ID: &str = "com.soujpg.pack.upscale_demo";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("dm-b1-{tag}-{}", std::process::id()));
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

/// 把仓库 `packs/plugins/imaging` 打成 `.dampack`，manifest 的 id 换成 `id`（其余原样）。
fn build_archive_as(out_dir: &Path, id: &str) -> PathBuf {
    let src = repo_root().join("packs/plugins/upscale");
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
    let pos = entries
        .iter()
        .position(|(n, _)| n == "manifest.json")
        .expect("归档必须含 manifest.json");
    let mut manifest: Value = serde_json::from_slice(&entries[pos].1).expect("仓库 manifest 合法");
    manifest["id"] = Value::String(id.to_string());
    entries[pos].1 = serde_json::to_vec_pretty(&manifest).expect("重写 manifest");

    std::fs::create_dir_all(out_dir).expect("建归档目录");
    let target = out_dir.join(format!("{id}-1.0.0.dampack"));
    let file = std::fs::File::create(&target).expect("建归档");
    pack_host::write_archive(file, &entries).expect("写归档失败");
    target
}

/// 验收 4：packs 每个插件行/版本行都带非空 `name`；映射外能力拿到的是 manifest 的名字；
/// specs 行带 `pluginStatus.pluginName`（fresh 库内置包还没进账本时能力面板靠它）。
#[tokio::test]
async fn packs_and_specs_rows_carry_manifest_names() {
    let (sys, app) = fresh_state("names");
    let archive = build_archive_as(&sys, DEMO_ID);
    let (code, body) = send(
        &app,
        "POST",
        "/api/compliance/packs/install",
        Some(serde_json::json!({ "path": archive.to_string_lossy(), "activate": true })),
    )
    .await;
    assert_eq!(code, 200, "装包失败：{body}");

    // ---- GET /api/compliance/packs：原文整段打到 stdout（报告里贴的就是它）
    let (code, list) = send(&app, "GET", "/api/compliance/packs", None).await;
    assert_eq!(code, 200, "列表接口失败：{list}");
    println!(
        "---- GET /api/compliance/packs ----\n{}",
        serde_json::to_string_pretty(&list).unwrap()
    );
    let rows = list["plugins"].as_array().expect("plugins 必须是数组");
    assert!(!rows.is_empty(), "装完包后 plugins 不该为空：{list}");
    for row in rows {
        let name = row["name"].as_str().unwrap_or("").trim().to_string();
        assert!(
            !name.is_empty(),
            "每个插件行都要带非空 name（界面不许退显裸 id）：{row}"
        );
        assert_ne!(
            name,
            row["id"].as_str().unwrap_or(""),
            "name 必须是用户面名字，不能是 id 兜底：{row}"
        );
        for v in row["versions"].as_array().cloned().unwrap_or_default() {
            if v["present"].as_bool() == Some(true) {
                let vn = v["name"].as_str().unwrap_or("").trim().to_string();
                assert!(!vn.is_empty(), "版本目录在时版本行也要带 name：{v}");
            }
        }
    }

    // ---- 映射外能力：名字来自包自己的 manifest（short_name），描述来自 summary
    let demo = rows
        .iter()
        .find(|r| r["id"].as_str() == Some(DEMO_ID))
        .unwrap_or_else(|| panic!("装了 {DEMO_ID} 却不在列表里：{list}"));
    assert_eq!(demo["name"], "图片放大", "能力名要取 manifest short_name：{demo}");
    assert_eq!(
        demo["summary"], "把小图放大 2~4 倍，细节更清楚",
        "一句话描述要取 manifest summary：{demo}"
    );
    assert_eq!(
        demo["runtime"]["available"], true,
        "装好就该可用（导航据此出现）：{demo}"
    );

    // ---- GET /api/specs：行里带能力名（fresh 库上内置判定包还没进账本，能力面板走这条补集）
    let (code, specs) = send(&app, "GET", "/api/specs", None).await;
    assert_eq!(code, 200, "规格列表失败：{specs}");
    println!(
        "---- GET /api/specs ----\n{}",
        serde_json::to_string_pretty(&specs).unwrap()
    );
    let items = specs["items"].as_array().expect("items 必须是数组");
    assert!(!items.is_empty(), "首次访问应惰性补默认方案：{specs}");
    for s in items {
        let pn = s["pluginStatus"]["pluginName"].as_str().unwrap_or("").trim();
        if s["pluginStatus"]["installed"].as_bool() == Some(true) {
            assert!(
                !pn.is_empty(),
                "能力在装时 specs 行必须带 pluginName（能力面板补集显示中文名）：{s}"
            );
        }
    }
    let first = &items[0];
    assert_eq!(
        first["pluginStatus"]["pluginName"], "合规检查",
        "内置判定包的能力名 = manifest short_name：{first}"
    );
    assert!(
        !first["pluginStatus"]["pluginSummary"]
            .as_str()
            .unwrap_or("")
            .trim()
            .is_empty(),
        "一句话描述也要下发：{first}"
    );
}

/// 既有 5 个随产品分发的包，**界面名不变**（对齐 `plugin-design-and-ui` §3 用户面命名表）。
///
/// 这条是 KI-36 的回归钉子：删掉前端硬编码映射表之后，「合规检查 / 房源图检查 / 抠图换背景 /
/// 图片放大 / 图片处理」这五个名字必须照样出现在界面上 —— 现在它们写在包自己的 manifest 里
/// （`short_name`），这条测试锁住「包数据 = 用户面名字」，谁改包名谁先看见红。
#[test]
fn builtin_five_packs_keep_their_user_facing_names() {
    let expected: [(&str, &str); 5] = [
        ("ecommerce", "合规检查"),
        ("realestate", "房源图检查"),
        ("segment", "抠图换背景"),
        ("upscale", "图片放大"),
        ("imaging", "图片处理"),
    ];
    for (dir_name, want) in expected {
        let dir = repo_root().join("packs/plugins").join(dir_name);
        let (m, _ident) = pack_host::load_pack(&dir)
            .unwrap_or_else(|e| panic!("仓库包 {dir_name} 必须能加载：{e}"));
        assert_eq!(m.short_label("zh-Hans"), want, "包 {dir_name} 的界面名变了（§3 命名表）");
        assert!(
            m.summary_text("zh-Hans").map(|s| !s.is_empty()).unwrap_or(false),
            "包 {dir_name} 必须带一句话描述（§3.1：每个功能入口配一句「做什么 + 有什么用」）"
        );
        // 名字与描述是**用户面**文案：不得出现内部词（与 ui_copy_guard 同一张红线表）
        for text in [m.short_label("zh-Hans"), m.summary_text("zh-Hans").unwrap_or("")] {
            for bad in ["插件", "规格", "manifest", "job", "verdict", "行业包"] {
                assert!(
                    !text.contains(bad),
                    "包 {dir_name} 的用户面文案里出现内部词「{bad}」：{text}"
                );
            }
        }
    }
}

#[test]
fn legacy_manifest_without_short_name_falls_back_to_name() {
    let repo_pack = repo_root().join("packs/plugins/imaging");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(repo_pack.join("manifest.json")).unwrap()).unwrap();
    let full_name = match &manifest["name"] {
        Value::String(s) => s.clone(),
        // i18n-F：对象形态（{"zh-Hans":…}）—— 老包回落断言对齐 zh-Hans 文案
        Value::Object(o) => o
            .get("zh-Hans")
            .and_then(|v| v.as_str())
            .expect("manifest.name 至少给一条非空文案")
            .to_string(),
        _ => panic!("manifest.name 必须是字符串或对象"),
    };
    // 删掉 DM-B1 新增的两个可选键 = 老包装进新宿主（api_version 不变，仍 1.1）
    let obj = manifest.as_object_mut().unwrap();
    obj.remove("short_name");
    obj.remove("summary");
    let dir = std::env::temp_dir().join(format!("dm-b1-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap())
        .unwrap();

    let (m, _ident) = pack_host::load_pack(&dir).expect("老 manifest（无 short_name/summary）必须照样能加载");
    assert_eq!(m.short_label("zh-Hans"), full_name, "没声明 short_name 时回落 manifest name");
    assert_eq!(m.summary_text("zh-Hans"), None, "没声明 summary 时不给描述（不编造）");
    let _ = std::fs::remove_dir_all(&dir);
}
