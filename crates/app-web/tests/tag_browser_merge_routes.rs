//! task（`/tags` 标签总览 + 批量合并/改名/删除）：HTTP 层门禁。
//!
//! 钉八件事（对应卡的验收 1-5）：
//! 1. `GET /library/tag-groups` 的分组值数 = `tags?page=1&key=` 的 total（同一真值）；
//!    只在回收站资产上出现的值不进值数（否则「左 3 值、右 2 行」）；
//! 2. `tags?page=1` 的三条分支：key / keyless（裸值）/ 无匹配 → 空集；三条件都不给 → 400
//!    （全量按计数排序 3.26s，不允许从这条路径进）；
//! 3. keyset 翻页：`(count DESC, value ASC)` 下 `afterCount/afterValue` 续页不重不漏
//!    （计数并列时要靠 value 升序断开，这是最容易写成「翻页丢一行」的地方）；
//! 4. 合并：多源 → 1 目标，affected = 去重后在库资产数，源值消失、资产上的标签为目标值，
//!    审计 `tag_merge_log` 逐源值一行；
//! 5. 合并冲突：同一张图已有目标值 → `uq(assetId,value)` 去重（不产生重复行，也不虚增计数）；
//! 6. 改名 = 单源合并到新值；
//! 7. 删除：有在库资产的值 → 400 + 可读原因（点名值与张数）；只剩回收站资产的值 → 真删；
//! 8. 样本图：`POST /library/tag-samples` 每值最多 N 个在库 assetId。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// 8 张资产（a5 在回收站）+ 7 个在库值：
/// - `尺寸`：滚筒_10KG(a1,a2) / 滚筒_5KG(a3)；`尺寸-旧款` 只在 a5（回收站）→ 不进左栏值数
/// - `颜色`：白(a2) / 纯白(a4,a7) / 白色(a6,a7) —— 纯白 与 白色 计数并列，专测 keyset 并列断开
/// - `风格`：极简(a4)；裸值 `Sunset`(a3)；裸值 `旧标签` 只在 a5（回收站）
/// - a7 同时有 纯白 + 白色（合并冲突：同一张图已有目标值）
fn seed(data_dir: &std::path::Path) {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let rows = [
        (1i64, "/lib/a1.jpg", 0),
        (2, "/lib/a2.jpg", 0),
        (3, "/lib/a3.jpg", 0),
        (4, "/lib/a4.jpg", 0),
        (5, "/lib/trash.jpg", 1),
        (6, "/lib/a6.jpg", 0),
        (7, "/lib/a7.jpg", 0),
        (8, "/lib/a8.jpg", 0),
    ];
    for (id, path, deleted) in rows {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt, deleted)
             VALUES (?1, ?2, 0, ?3, 'jpg', 1, 0, ?1, ?4)",
            rusqlite::params![id, path, path.rsplit('/').next().unwrap(), deleted],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1,'primary',0,1,1)",
            [id],
        )
        .unwrap();
    }
    let tags = [
        (1i64, "尺寸-滚筒_10KG"),
        (2, "尺寸-滚筒_10KG"),
        (2, "颜色-白"),
        (3, "尺寸-滚筒_5KG"),
        (3, "Sunset"),
        (4, "风格-极简"),
        (4, "颜色-纯白"),
        (7, "颜色-纯白"),
        (7, "颜色-白色"),
        (6, "颜色-白色"),
        (5, "尺寸-旧款"),
        (5, "旧标签"),
    ];
    for (asset, value) in tags {
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, ?2, 'manual', 1.0, 1)",
            rusqlite::params![asset, value],
        )
        .unwrap();
    }
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dam-task-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn app_of(data_dir: &std::path::Path) -> axum::Router {
    let state = app_web::test_support::state(
        data_dir.to_path_buf(),
        core_storage::open(&data_dir.join("dam.db")).unwrap(),
    );
    app_web::router_without_dist(state)
}

/// 百分号编码（query 里的中文/游标值都从这条路过，测试不引额外依赖）
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
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
    let status = res.status();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn values_of(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["value"].as_str().unwrap().to_string())
        .collect()
}

fn group_of(v: &Value, key: &str) -> i64 {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"] == json!(key))
        .unwrap_or_else(|| panic!("缺少分组 {key}: {v}"))["values"]
        .as_i64()
        .unwrap()
}

/// 验收 2：左栏 key 分组的「值数」必须等于 `tags?page=1&key=` 的 total（两处数字对比）。
#[tokio::test]
async fn tag_groups_values_match_key_page_total() {
    let dir = fresh_dir("groups");
    seed(&dir);
    let app = app_of(&dir);

    let (code, g) = get(&app, "/api/library/tag-groups").await;
    assert_eq!(code, StatusCode::OK, "{g}");
    // 裸值组 key='' 必须存在（Sunset；旧标签 只在回收站 → 不计）
    assert_eq!(group_of(&g, ""), 1, "裸值组只算 Sunset: {g}");
    assert_eq!(group_of(&g, "颜色"), 3, "{g}");
    assert_eq!(
        group_of(&g, "尺寸"),
        2,
        "尺寸-旧款 只在回收站，不进值数: {g}"
    );
    assert_eq!(group_of(&g, "风格"), 1, "{g}");
    assert_eq!(g["total"], json!(7), "在库值共 7 个: {g}");

    // 逐组与值列表 total 对账
    for (key, want) in [("尺寸", 2), ("颜色", 3), ("风格", 1)] {
        let (code, v) = get(&app, &format!("/api/library/tags?page=1&key={}", enc(key))).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(
            v["total"],
            json!(want),
            "key={key} 的 total 与左栏值数一致: {v}"
        );
        assert_eq!(values_of(&v).len() as i64, want, "{v}");
    }
    // 裸值组：keyless 页的 total 与左栏 '' 组一致
    let (code, v) = get(&app, "/api/library/tags?page=1&keyless=1").await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["total"], json!(1), "裸值页 total = 左栏 '' 组: {v}");
    assert_eq!(values_of(&v), vec!["Sunset"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 1 的守卫：`page=1` 不带任何范围条件 → 400（全量按计数排序代价 3.26s，别从这条路进）。
#[tokio::test]
async fn page_mode_requires_a_scope() {
    let dir = fresh_dir("noscope");
    seed(&dir);
    let app = app_of(&dir);
    let (code, v) = get(&app, "/api/library/tags?page=1").await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "必须拒绝无范围的全量请求: {v}"
    );
    assert!(v["error"].as_str().unwrap().contains("key"), "{v}");

    // 不相交（key=尺寸 × prefix=Sunset）→ 空集不报错
    let (code, v) = get(
        &app,
        &format!("/api/library/tags?page=1&key={}&prefix=Sunset", enc("尺寸")),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(0), "不相交范围 = 空集: {v}");

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 1/3：keyset 翻页不重不漏（计数并列也要靠 value 升序断开）。
#[tokio::test]
async fn keyset_paging_no_dup_no_loss() {
    let dir = fresh_dir("keyset");
    seed(&dir);
    let app = app_of(&dir);

    // key=颜色 三行，计数 白色=2 / 纯白=2 / 白=1 → 并列按 value ASC 断开
    let (_, p1) = get(
        &app,
        &format!("/api/library/tags?page=1&key={}&limit=2", enc("颜色")),
    )
    .await;
    assert_eq!(values_of(&p1), vec!["颜色-白色", "颜色-纯白"], "{p1}");
    assert_eq!(p1["total"], json!(3));
    assert_eq!(p1["hasMore"], json!(true), "{p1}");
    let last = &p1["items"].as_array().unwrap()[1];
    assert_eq!(last["count"], json!(2), "并列计数用于游标: {p1}");

    let (_, p2) = get(
        &app,
        &format!(
            "/api/library/tags?page=1&key={}&limit=2&afterCount={}&afterValue={}",
            enc("颜色"),
            last["count"].as_i64().unwrap(),
            enc(last["value"].as_str().unwrap())
        ),
    )
    .await;
    assert_eq!(values_of(&p2), vec!["颜色-白"], "续页只补剩下的 1 行: {p2}");
    assert_eq!(p2["hasMore"], json!(false), "{p2}");
    assert_eq!(p2["total"], json!(3), "total 不随翻页变: {p2}");

    // 拼起来 = 全量（无重复）
    let all: Vec<String> = values_of(&p1).into_iter().chain(values_of(&p2)).collect();
    let uniq: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(all.len(), uniq.len(), "翻页出现重复行: {all:?}");
    assert_eq!(all.len(), 3);

    // keyless 分支同样支持 keyset（按 value ASC）
    let (_, k1) = get(&app, "/api/library/tags?page=1&keyless=1&limit=1").await;
    assert_eq!(values_of(&k1), vec!["Sunset"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 4/5：合并 —— affected 去重、源值消失、目标值承接所有资产、冲突不重复、审计落库。
#[tokio::test]
async fn merge_sources_into_target_with_dedup_and_audit() {
    let dir = fresh_dir("merge");
    seed(&dir);
    let app = app_of(&dir);

    // 合并前：白色 2 张（a6,a7）、纯白 2 张（a4,a7）、白 1 张（a2）
    let (_, before) = get(
        &app,
        &format!("/api/library/tags?page=1&key={}", enc("颜色")),
    )
    .await;
    assert_eq!(before["total"], json!(3));

    let (code, out) = post(
        &app,
        "/api/library/tags/merge",
        json!({ "values": ["颜色-白", "颜色-纯白"], "into": "颜色-白色" }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    // 受影响资产 = a2,a4,a7 去重（不是 1+2 行数）；a7 已有目标值 → INSERT OR IGNORE 吃掉
    assert_eq!(out["affected"], json!(3), "{out}");
    assert_eq!(out["keptValue"], json!("颜色-白色"));
    let mut removed: Vec<String> = out["removedValues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    removed.sort();
    assert_eq!(removed, vec!["颜色-白", "颜色-纯白"]);

    // 合并后：颜色 只剩 1 个值，承接 a2,a4,a6,a7 = 4 张
    let (_, after) = get(
        &app,
        &format!("/api/library/tags?page=1&key={}", enc("颜色")),
    )
    .await;
    assert_eq!(after["total"], json!(1), "旧值从列表消失: {after}");
    assert_eq!(values_of(&after), vec!["颜色-白色"]);
    assert_eq!(
        after["items"][0]["count"],
        json!(4),
        "目标值承接全部资产: {after}"
    );

    // 资产上的标签真的变成目标值（不是只改列表）
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE value='颜色-白色' AND assetId IN (2,4,6,7)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 4);
    let leftovers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE value IN ('颜色-白','颜色-纯白')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftovers, 0);
    // a7 不会出现两行同值（uq(assetId,value)）
    let dup: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE assetId=7 AND value='颜色-白色'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dup, 1);

    // 审计：逐源值一行，toValue = 目标值
    let logs: Vec<(String, String, i64)> = conn
        .prepare("SELECT fromValue, toValue, affected FROM tag_merge_log ORDER BY fromValue")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(logs.len(), 2, "逐源值一行审计: {logs:?}");
    assert!(logs.iter().all(|(_, to, _)| to == "颜色-白色"));
    // 每个源的 affected 记的是「本源值合并前有多少在库资产」
    let by_src: std::collections::HashMap<_, _> =
        logs.iter().map(|(f, _, a)| (f.as_str(), *a)).collect();
    assert_eq!(by_src.get("颜色-白"), Some(&1), "{logs:?}");
    assert_eq!(by_src.get("颜色-纯白"), Some(&2), "{logs:?}");
    drop(conn);

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 4：预览只算不写，且与执行回执同一个数字（确认框口径）。
#[tokio::test]
async fn merge_preview_is_read_only_and_matches_execute() {
    let dir = fresh_dir("preview");
    seed(&dir);
    let app = app_of(&dir);

    let (code, prev) = post(
        &app,
        "/api/library/tags/merge-preview",
        json!({ "values": ["颜色-白", "颜色-纯白"], "into": "颜色-白色" }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{prev}");
    assert_eq!(prev["affected"], json!(3), "去重后 3 张: {prev}");
    assert_eq!(prev["removedValues"].as_array().unwrap().len(), 2, "{prev}");

    // 只算不写
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let untouced: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE value IN ('颜色-白','颜色-纯白')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(untouced, 3, "预览不得改动数据（白1 + 纯白2）");
    let logs: i64 = conn
        .query_row("SELECT COUNT(*) FROM tag_merge_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logs, 0, "预览不得写审计");
    drop(conn);

    // 执行回执与预览同一个数
    let (code, out) = post(
        &app,
        "/api/library/tags/merge",
        json!({ "values": ["颜色-白", "颜色-纯白"], "into": "颜色-白色" }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    assert_eq!(out["affected"], prev["affected"], "预览与执行必须同数");

    // 目标值不在源列表里也要能算（新增值的改名场景）
    let (code, prev) = post(
        &app,
        "/api/library/tags/merge-preview",
        json!({ "values": ["Sunset"], "into": "场景-夕阳" }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{prev}");
    assert_eq!(prev["affected"], json!(1));
    assert_eq!(prev["keptValue"], json!("场景-夕阳"));

    // 源与目标完全相同 → 400（没有可合并的源）
    let (code, _) = post(
        &app,
        "/api/library/tags/merge-preview",
        json!({ "values": ["Sunset"], "into": "Sunset" }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 4：改名 = 单源合并到新值（含新增值：目标值原本不存在）。
#[tokio::test]
async fn rename_is_single_source_merge() {
    let dir = fresh_dir("rename");
    seed(&dir);
    let app = app_of(&dir);

    let (code, out) = post(
        &app,
        "/api/library/tags/rename",
        json!({ "from": "风格-极简", "to": "风格-简约" }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    assert_eq!(out["affected"], json!(1));
    assert_eq!(out["keptValue"], json!("风格-简约"));

    let (_, v) = get(
        &app,
        &format!("/api/library/tags?page=1&key={}", enc("风格")),
    )
    .await;
    assert_eq!(values_of(&v), vec!["风格-简约"], "{v}");

    // 改名到空值 / 同名 → 400（可读错误，不是 500）
    let (code, v) = post(
        &app,
        "/api/library/tags/rename",
        json!({ "from": "风格-简约", "to": "  " }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    let (code, v) = post(
        &app,
        "/api/library/tags/rename",
        json!({ "from": "风格-简约", "to": "风格-简约" }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "同名改名 = 无源可合并: {v}");

    std::fs::remove_dir_all(&dir).ok();
}

/// 验收 5：删除只允许「在库资产数为 0」的值；有资产 → 400 + 点名原因。
#[tokio::test]
async fn delete_refuses_values_with_assets() {
    let dir = fresh_dir("delete");
    seed(&dir);
    let app = app_of(&dir);

    // 有 4 张在库资产 → 拒绝，且错误文案点名值与张数
    let (code, v) = post(
        &app,
        "/api/library/tags/delete",
        json!({ "values": ["颜色-白色"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    let msg = v["error"].as_str().unwrap();
    assert!(
        msg.contains("颜色-白色") && msg.contains('2'),
        "可读原因要带值和张数: {msg}"
    );
    assert!(msg.contains("合并"), "要提示先合并: {msg}");

    // 混合请求（一个能删一个不能）→ 整批拒绝，不能半删
    let (code, v) = post(
        &app,
        "/api/library/tags/delete",
        json!({ "values": ["旧标签", "颜色-白色"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let still: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE value='旧标签'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still, 1, "整批拒绝时不能半删");

    // 只剩回收站资产的值 → 真删（连带删掉回收站里的行）
    let (code, out) = post(
        &app,
        "/api/library/tags/delete",
        json!({ "values": ["旧标签", "尺寸-旧款"] }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    assert_eq!(out["removedRows"], json!(2), "{out}");
    let mut deleted: Vec<String> = out["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    deleted.sort();
    assert_eq!(deleted, vec!["尺寸-旧款", "旧标签"]);
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE value IN ('旧标签','尺寸-旧款')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    // 审计留痕（kind='delete'）
    let kinds: Vec<String> = conn
        .prepare("SELECT kind FROM tag_merge_log WHERE kind='delete'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(kinds.len(), 2, "两个值各一行删除审计");
    drop(conn);

    std::fs::remove_dir_all(&dir).ok();
}

/// 每值样本图：在库、最多 limit 个、值不存在 → 空数组（前端显示占位，不报错）。
#[tokio::test]
async fn samples_are_live_assets_only() {
    let dir = fresh_dir("samples");
    seed(&dir);
    let app = app_of(&dir);

    let (code, v) = post(
        &app,
        "/api/library/tag-samples",
        json!({ "values": ["颜色-白色", "不存在的值"], "limit": 3 }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["value"], json!("颜色-白色"));
    let ids: Vec<i64> = items[0]["assetIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "白色 只有 a6,a7 两张: {v}");
    assert!(ids.iter().all(|id| *id == 6 || *id == 7), "{ids:?}");
    assert_eq!(items[1]["assetIds"], json!([]));

    // 空 values → 400
    let (code, _) = post(&app, "/api/library/tag-samples", json!({ "values": [] })).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(&dir).ok();
}
