//! task（BATCH-07 B07-8 代码层门禁）：`/api/library/tags` 前缀/key/offset 范围扫描。
//!
//! R40（KI-13）：侧栏/检查器/批量打标的标签输入此前是裸 input，候选只在搜索页自写一份。
//! 本卡把候选统一到 `prefix= / key= / offset=` 三个参数上，钉五件事：
//! 1. `prefix=`：范围扫描语义（非 LIKE）、计数 = 明细真值、`key` 字段（第一个 `-` 前段）、
//!    ASCII 大小写不敏感保持、字面 `%` 不再是通配符（无 LIKE 即无转义问题）；
//! 2. `key=`：`value ∈ ['key-', 'key-'‖U+10FFFF)`，与 prefix 求交（不相交 → 空集不报错）；
//! 3. `offset=`：`(count DESC, value ASC)` 稳定排序下的位移翻页，total 不随分页变；
//! 4. 无参：旧数组 `[{value,count}]` 形态原样保持（侧栏标签云/智能文件夹零改动）；
//! 5. EXPLAIN 三态：prefix / key / key+prefix 的候选查询必须是
//!    `SEARCH … USING (COVERING )?INDEX idx_tag_value_nocase (value>? AND value<?)`，
//!    出现 `SCAN tag` = 断言失败（LIKE 前缀实测退化为覆盖索引全扫 220ms vs 范围 28ms）。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// 6 张资产：a1/a2 带 `尺寸-滚筒_10KG`，a3 带 `尺寸-滚筒_5KG` + `Sunset`，a4 带 `风格-极简`，
/// a5（deleted=1）带 `尺寸-旧款`（软删资产的标签不得进候选/计数），a6 不带标签。
fn seed(data_dir: &std::path::Path) {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let rows = [
        (1i64, "/lib/roller1.jpg", 0),
        (2, "/lib/roller2.jpg", 0),
        (3, "/lib/small.jpg", 0),
        (4, "/lib/plaza.jpg", 0),
        (5, "/lib/deleted.jpg", 1),
        (6, "/lib/plain.jpg", 0),
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
        (3, "尺寸-滚筒_5KG"),
        (3, "Sunset"),
        (4, "风格-极简"),
        (5, "尺寸-旧款"),
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

fn values_of(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["value"].as_str().unwrap().to_string())
        .collect()
}

fn app_of(data_dir: &std::path::Path) -> axum::Router {
    let state = app_web::test_support::state(
        data_dir.to_path_buf(),
        core_storage::open(&data_dir.join("dam.db")).unwrap(),
    );
    app_web::router_without_dist(state)
}

#[tokio::test]
async fn prefix_range_candidates_with_key_field_and_truth_counts() {
    let data_dir = fresh_dir("prefix");
    seed(&data_dir);
    let app = app_of(&data_dir);

    // 前缀候选：计数 = 明细真值（10KG×2 排前）；deleted=1 的 尺寸-旧款 不进 total
    let (code, v) = call(&app, "/api/library/tags?prefix=%E5%B0%BA%E5%AF%B8&limit=30").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(2), "尺寸 前缀下 2 个值（软删不计）: {v}");
    assert_eq!(v["hasMore"], json!(false));
    assert_eq!(values_of(&v), vec!["尺寸-滚筒_10KG", "尺寸-滚筒_5KG"]);
    // key 字段：key-val 形态给前段，裸值给 null
    assert_eq!(v["items"][0]["key"], json!("尺寸"));
    assert_eq!(v["items"][0]["count"], json!(2));
    assert_eq!(v["items"][1]["key"], json!("尺寸"));
    assert_eq!(v["items"][1]["count"], json!(1));

    // ASCII 大小写不敏感保持（NOCASE 范围）；裸值 key=null
    let (_, v) = call(&app, "/api/library/tags?prefix=sun").await;
    assert_eq!(v["total"], json!(1), "ASCII 前缀不分大小写: {v}");
    assert_eq!(values_of(&v), vec!["Sunset"]);
    assert_eq!(v["items"][0]["key"], json!(null), "裸值（无 -）key 为 null");

    // 字面 % / 下划线不是通配符（范围语义天然如此）
    let (_, v) = call(&app, "/api/library/tags?prefix=%25").await;
    assert_eq!(v["total"], json!(0), "字面 % 非通配符: {v}");
    let (_, v) = call(&app, "/api/library/tags?prefix=%E5%B0%BA%E5%AF%B8_%").await;
    assert_eq!(v["total"], json!(0), "字面 _ 非通配符: {v}");

    // 无匹配：total=0 + 空数组（不是 5xx，前端据此显示空态）
    let (_, v) = call(&app, "/api/library/tags?prefix=%E4%B8%8D%E5%AD%98%E5%9C%A8").await;
    assert_eq!(v["total"], json!(0));
    assert_eq!(values_of(&v), Vec::<String>::new());

    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn key_scoped_candidates_and_prefix_intersection() {
    let data_dir = fresh_dir("key");
    seed(&data_dir);
    let app = app_of(&data_dir);

    // key=尺寸 → 该 key 下全部值（等价 尺寸- 前缀范围）
    let (code, v) = call(&app, "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&limit=30").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(2), "key=尺寸 下 2 个值: {v}");
    assert_eq!(values_of(&v), vec!["尺寸-滚筒_10KG", "尺寸-滚筒_5KG"]);

    // key 语义 ≠ prefix：key=sun 找的是 `sun-` 前缀，裸值 Sunset 不算该 key 的值
    let (_, v) = call(&app, "/api/library/tags?key=sun").await;
    assert_eq!(v["total"], json!(0), "key= 要求 key- 前缀形态，裸值不算: {v}");

    // key + prefix 组合（在 key 内按 prefix 收窄）
    let (_, v) = call(
        &app,
        "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&prefix=%E5%B0%BA%E5%AF%B8-%E6%BB%9A%E7%AD%92_1",
    )
    .await;
    assert_eq!(v["total"], json!(1), "key=尺寸 × prefix=尺寸-滚筒_1 → 仅 10KG: {v}");
    assert_eq!(values_of(&v), vec!["尺寸-滚筒_10KG"]);

    // 范围不相交（key=尺寸 × prefix=Sunset）→ 空集，不报错
    let (code, v) = call(&app, "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&prefix=Sunset").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], json!(0), "不相交范围 = 空集: {v}");
    assert_eq!(values_of(&v), Vec::<String>::new());

    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn offset_paging_over_stable_order() {
    let data_dir = fresh_dir("offset");
    seed(&data_dir);
    let app = app_of(&data_dir);

    // (count DESC, value ASC) 稳定排序：key=尺寸 的 2 页
    let (_, v) = call(&app, "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&limit=1&offset=0").await;
    assert_eq!(values_of(&v), vec!["尺寸-滚筒_10KG"]);
    assert_eq!(v["total"], json!(2), "total 不随分页变");
    assert_eq!(v["hasMore"], json!(true));
    let (_, v) = call(&app, "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&limit=1&offset=1").await;
    assert_eq!(values_of(&v), vec!["尺寸-滚筒_5KG"], "第 2 页 = 次序第 2 项");
    assert_eq!(v["hasMore"], json!(false));
    // 翻过尾：空数组、total 仍是真值
    let (_, v) = call(&app, "/api/library/tags?key=%E5%B0%BA%E5%AF%B8&limit=1&offset=2").await;
    assert_eq!(values_of(&v), Vec::<String>::new());
    assert_eq!(v["total"], json!(2));

    // 仅 offset（无 prefix/key）= 全量候选翻页（count DESC 序）；软删资产的标签不进计数
    let (_, v) = call(&app, "/api/library/tags?limit=2&offset=2").await;
    assert_eq!(v["total"], json!(4), "全库 4 个活标签值: {v}");
    let page: Vec<String> = values_of(&v);
    assert_eq!(page.len(), 2, "第 2 页补齐剩余: {v}");

    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn no_param_keeps_legacy_array_shape() {
    let data_dir = fresh_dir("legacy");
    seed(&data_dir);
    let app = app_of(&data_dir);

    let (code, v) = call(&app, "/api/library/tags").await;
    assert_eq!(code, StatusCode::OK);
    assert!(v.is_array(), "无参必须维持旧数组形态: {v}");
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 4, "默认 limit=30 取全部 4 个活值: {v}");
    assert_eq!(arr[0]["value"], json!("尺寸-滚筒_10KG"), "count DESC 在前");
    assert!(arr[0].get("key").is_none(), "旧形态不带 key 字段（加字段不破契约，但这里证明没误改形态）");

    std::fs::remove_dir_all(&data_dir).ok();
}

/// EXPLAIN 三态：prefix / key / key+prefix 都必须范围索引检索（SEARCH），SCAN = 失败。
/// 与 svc-library::tag_candidates 的有界分支同形（边界由参数绑定）。
#[test]
fn explain_query_plan_range_search_no_scan() {
    let data_dir = fresh_dir("explain");
    seed(&data_dir);
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();

    let sql = |lo: &str, hi: &str| {
        format!(
            "EXPLAIN QUERY PLAN SELECT t.value, COUNT(DISTINCT t.assetId) AS n
             FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value >= '{lo}' COLLATE NOCASE AND t.value < '{hi}' COLLATE NOCASE
             GROUP BY t.value ORDER BY n DESC, t.value"
        )
    };
    let cases: [(&str, String); 3] = [
        // prefix=尺寸 → ['尺寸', '尺寸'‖U+10FFFF)
        ("prefix", sql("尺寸", "尺寸\u{10FFFF}")),
        // key=尺寸 → ['尺寸-', '尺寸-'‖U+10FFFF)
        ("key", sql("尺寸-", "尺寸-\u{10FFFF}")),
        // key=尺寸 × prefix=尺寸-滚筒_1 → 求交 ['尺寸-滚筒_1', '尺寸-滚筒_1'‖U+10FFFF)
        ("key+prefix", sql("尺寸-滚筒_1", "尺寸-滚筒_1\u{10FFFF}")),
    ];
    for (name, q) in cases {
        let plans: Vec<String> = conn
            .prepare(&q)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let joined = plans.join("\n");
        assert!(
            joined.contains("idx_tag_value_nocase"),
            "{name}: 候选必须走 0024 nocase 索引，实际计划:\n{joined}"
        );
        assert!(
            joined.contains("SEARCH t USING INDEX") && joined.contains("value>? AND value<?"),
            "{name}: 必须是范围 SEARCH（value>? AND value<?），实际计划:\n{joined}"
        );
        assert!(
            !joined.to_lowercase().contains("scan t") && !joined.to_lowercase().contains("scan tag"),
            "{name}: 候选查询不允许任何全表/全索引扫，实际计划:\n{joined}"
        );
    }

    std::fs::remove_dir_all(&data_dir).ok();
}

/// 生产形态（参数绑定 `?1 COLLATE NOCASE`）也必须命中同一范围 SEARCH —— 上面用字面量，
/// 这里补参数版，避免「字面量走索引、绑定参数退化成扫」的方言差异。
#[test]
fn explain_query_plan_bound_params_also_range_search() {
    let data_dir = fresh_dir("explain-bind");
    seed(&data_dir);
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let q = "EXPLAIN QUERY PLAN SELECT t.value, COUNT(DISTINCT t.assetId) AS n
             FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE
             GROUP BY t.value ORDER BY n DESC, t.value LIMIT ?3 OFFSET ?4";
    let plans: Vec<String> = conn
        .prepare(q)
        .unwrap()
        .query_map(
            rusqlite::params!["尺寸", "尺寸\u{10FFFF}", 30i64, 0i64],
            |r| r.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let joined = plans.join("\n");
    assert!(
        joined.contains("idx_tag_value_nocase")
            && joined.contains("SEARCH t USING INDEX")
            && joined.contains("value>? AND value<?"),
        "绑定参数形态同样必须范围 SEARCH，实际计划:\n{joined}"
    );
    std::fs::remove_dir_all(&data_dir).ok();
}
