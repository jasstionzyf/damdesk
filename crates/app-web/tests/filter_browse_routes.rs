//! task（BATCH-07 B07-1/B07-2 代码层门禁）：搜索域「纯过滤浏览」通道 + 标签匹配语义。
//!
//! 真机缺陷：搜索页 query 为空时按钮灰置（纯过滤无入口）、标签过滤 `value = ?`
//! 全值精确（key-val 形态下输入自然词必 0 命中）、0 命中与「无匹配」不可区分。
//! 这里走真路由钉四件事：
//! 1. 纯过滤通道的契约本体 = `GET /api/library/assets?tag=&dirPrefix=&ext=`（total 是唯一真值）；
//! 2. 标签匹配：全值精确 + 入口归一化（trim/空白折叠）+ 大小写不敏感（COLLATE NOCASE）；
//! 3. 候选提示：`GET /api/library/tags?prefix=` 前缀语义（旧数组形态不被破坏）；
//! 4. 索引：EXPLAIN QUERY PLAN 证明过滤/前缀都走 0024 的 nocase 索引，不是全表扫。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// 4 张资产：a/b 带 `尺寸-滚筒_10KG`（a 在 /lib/aa、b 在 /lib/bb，a 是 jpg、b 是 png），
/// c 带 `尺寸-滚筒_5KG` + `Sunset`，d 不带标签（在 /lib/aa）。
fn seed(data_dir: &std::path::Path) {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let rows = [
        (1i64, "/lib/aa/roller.jpg", "roller.jpg", "jpg"),
        (2, "/lib/bb/roller.png", "roller.png", "png"),
        (3, "/lib/aa/small.jpg", "small.jpg", "jpg"),
        (4, "/lib/aa/plain.jpg", "plain.jpg", "jpg"),
    ];
    for (id, path, name, ext) in rows {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt, deleted)
             VALUES (?1, ?2, 0, ?3, ?4, 1, 0, ?1, 0)",
            rusqlite::params![id, path, name, ext],
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
        (3, "尺寸-滚筒_5KG"),
        (3, "Sunset"),
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
    let d = std::env::temp_dir().join(format!("dam-t4f145794-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn call(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn ids_of(v: &Value) -> Vec<i64> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["assetId"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn pure_filter_browse_channel() {
    let data_dir = fresh_dir("browse");
    seed(&data_dir);
    let state =
        app_web::test_support::state(data_dir.clone(), core_storage::open(&data_dir.join("dam.db")).unwrap());
    let app = app_web::router_without_dist(state);

    // 全值精确：命中 a/b（B07-2①）；列表序 = importedAt DESC、assetId DESC
    let (code, v) = call(&app, "/api/library/assets?tag=%E5%B0%BA%E5%AF%B8-%E6%BB%9A%E7%AD%92_10KG&limit=10").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(2), "全值精确命中 2 张: {v}");
    assert_eq!(ids_of(&v), vec![2, 1]);

    // 大小写/首尾空格/内部空格归一化不影响命中（B07-2④）
    let (_, v) = call(&app, "/api/library/assets?tag=%E5%B0%BA%E5%AF%B8-%E6%BB%9A%E7%AD%92_10kg&limit=10").await;
    assert_eq!(v["total"], json!(2), "小写也命中（NOCASE）: {v}");
    let (_, v) = call(&app, "/api/library/assets?tag=%20%E5%B0%BA%E5%AF%B8-%E6%BB%9A%E7%AD%92%2010KG%20&limit=10").await;
    assert_eq!(v["total"], json!(2), "首尾+内部空格归一化后命中: {v}");

    // 自然词（非全值）不命中——过滤路径语义仍是精确，0 命中要可见（不是静默退化成整库）
    let (_, v) = call(&app, "/api/library/assets?tag=%E6%BB%9A%E7%AD%92&limit=10").await;
    assert_eq!(v["total"], json!(0));
    assert_eq!(ids_of(&v), Vec::<i64>::new(), "0 命中必须是空集");

    // 组合过滤：目录前缀 + 扩展名（B07-1 的组合分支）；列表序 = importedAt DESC
    let (_, v) = call(&app, "/api/library/assets?dirPrefix=%2Flib%2Faa&ext=jpg&limit=10").await;
    assert_eq!(v["total"], json!(3), "dir+ext 组合: {v}");
    assert_eq!(ids_of(&v), vec![4, 3, 1]);
    // 组合 + NOCASE：sunset 命中 Sunset（大小写不敏感贯穿组合条件）
    let (_, v) = call(&app, "/api/library/assets?dirPrefix=%2Flib%2Faa&ext=jpg&tag=sunset&limit=10").await;
    assert_eq!(v["total"], json!(1), "组合条件 + 大小写不敏感命中 Sunset: {v}");
    assert_eq!(ids_of(&v), vec![3]);
    let (_, v) = call(&app, "/api/library/assets?dirPrefix=%2Flib%2Faa&tag=%E4%B8%8D%E5%AD%98%E5%9C%A8&limit=10").await;
    assert_eq!(v["total"], json!(0), "真 0 命中 = 空集: {v}");

    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn tag_prefix_candidates_endpoint() {
    let data_dir = fresh_dir("prefix");
    seed(&data_dir);
    let state =
        app_web::test_support::state(data_dir.clone(), core_storage::open(&data_dir.join("dam.db")).unwrap());
    let app = app_web::router_without_dist(state);

    // 前缀候选（B07-2②）：key 前缀 → 该 key 下全部值 + 计数 + total/hasMore
    let (code, v) = call(&app, "/api/library/tags?prefix=%E5%B0%BA%E5%AF%B8&limit=30").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(2), "尺寸 前缀下 2 个值: {v}");
    assert_eq!(v["hasMore"], json!(false));
    let values: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, vec!["尺寸-滚筒_10KG", "尺寸-滚筒_5KG"]);
    let counts: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, vec![2, 1], "频次随行（10KG×2、5KG×1）: {v}");

    // 大小写不敏感的前缀 + 转义：用户输入里的 %/_ 是字面量
    let (_, v) = call(&app, "/api/library/tags?prefix=sun").await;
    assert_eq!(v["total"], json!(1), "ASCII 前缀不分大小写: {v}");
    let (_, v) = call(&app, "/api/library/tags?prefix=%25").await;
    assert_eq!(v["total"], json!(0), "字面 % 不是通配符: {v}");
    let (_, v) = call(&app, "/api/library/tags?prefix=%E4%B8%8D%E5%AD%98%E5%9C%A8").await;
    assert_eq!(v["total"], json!(0), "无匹配候选也要给 total=0（前端据此显示空态）: {v}");

    // limit 生效 + hasMore 探针
    let (_, v) = call(&app, "/api/library/tags?prefix=%E5%B0%BA%E5%AF%B8&limit=1").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["total"], json!(2));
    assert_eq!(v["hasMore"], json!(true));

    // 旧形态兼容：不带 prefix 仍是数组（侧栏标签云/智能文件夹零改动）；种子共 3 个不同值
    let (code, v) = call(&app, "/api/library/tags?limit=30").await;
    assert_eq!(code, StatusCode::OK);
    assert!(v.is_array(), "不带 prefix 必须维持数组形态: {v}");
    assert_eq!(v.as_array().unwrap().len(), 3);

    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn explain_query_plan_tag_filter_uses_index() {
    let data_dir = fresh_dir("explain");
    seed(&data_dir);
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();

    // 过滤路径的子查询（与 svc-library::filter_where / engine-search::candidate_sql 同形）
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT a.assetId FROM assets a
             WHERE a.deleted = 0 AND a.assetId IN
             (SELECT assetId FROM tag WHERE value = '尺寸-滚筒_10KG' COLLATE NOCASE)",
        )
        .unwrap();
    let plans: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let joined = plans.join("\n");
    assert!(
        joined.contains("idx_tag_value_nocase"),
        "过滤子查询必须走 0024 nocase 索引，实际计划:\n{joined}"
    );
    assert!(
        !joined.to_lowercase().contains("scan tag"),
        "tag 不允许全表扫，实际计划:\n{joined}"
    );

    // 候选路径的前缀（svc-library::tag_candidates 同形：task 起 LIKE → 显式范围扫描 +
    // CROSS JOIN 钉连接顺序，SEARCH idx_tag_value_nocase (value>? AND value<?)；
    // LIKE 实测退化为覆盖索引全扫 220ms vs 范围 28ms）
    let mut stmt = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT t.value, COUNT(DISTINCT t.assetId) AS n
             FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value >= '尺寸' COLLATE NOCASE AND t.value < '尺寸{max}' COLLATE NOCASE
             GROUP BY t.value ORDER BY n DESC, t.value",
            max = '\u{10FFFF}'
        ))
        .unwrap();
    let plans: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let joined = plans.join("\n");
    assert!(
        joined.contains("idx_tag_value_nocase"),
        "前缀候选必须经 0024 nocase 索引做范围检索，实际计划:\n{joined}"
    );
    assert!(
        !joined.to_lowercase().contains("scan "),
        "候选查询不允许任何全表/全索引扫，实际计划:\n{joined}"
    );

    std::fs::remove_dir_all(&data_dir).ok();
}
