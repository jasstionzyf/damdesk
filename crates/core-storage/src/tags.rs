//! TAG-VALUE（2026-10-19 裁决）：标签值 DAO。
//!
//! 模型分层（唯一事实源）：
//! * `custom_tag_task` —— 打标任务定义：`name` = 维度名（用户起的），`prompt` = 问什么。
//! * `tag_result`       —— 任务结果表：每「资产 × 任务」一行，`name` = 维度名快照，
//!                         `value` = 模型原始输出（多值不拆，保留原串）。
//! * `tag`              —— 标签实体：**只有 value，没有 name**。结果 value 非空即自动
//!                         扇出（按「;」「；」换行拆分）成标签行，`origin` 记来源
//!                         （auto=模板打标 / manual=人工 / path=目录 / custom=自定义任务模型），
//!                         `model`+`taskId` 记溯源。
//!
//! 去重与优先级：`uq_tag_asset_value(assetId, value)` 同图同值只留一行；
//! 人工（manual）优先——写 auto/custom/path 都不覆盖已有 manual 行，人工写入则升级来源。

use core_types::TagResultRecord;
use rusqlite::Connection;

use crate::{Result, StorageError};

/// 标签值长度上限（与任务名/人工标签校验一致）。
pub const TAG_VALUE_MAX_CHARS: usize = 64;

/// 把字符串拆成标签值：分隔符 `;` `；` 换行，trim、去空、去重、按字符边界截断。
/// 仅兼容旧 provider 字符串输出（tag_result 里存的历史原文 / 旧提案 value）；
/// 统一标签主路径已改数组（服务端直出数组 → [`fanout_values`]），不再靠分隔符猜。
pub fn split_values(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split([';', '；', '\n', '\r']) {
        let v = piece.trim().trim_matches('"').trim();
        if v.is_empty() {
            continue;
        }
        let v: String = v.chars().take(TAG_VALUE_MAX_CHARS).collect();
        if !out.iter().any(|x| *x == v) {
            out.push(v);
        }
    }
    out
}

/// 统一标签（2026-10-19 裁决第 6 条）值归一化：trim、去引号、去句末标点、
/// 内部连续空白折叠成单个 `_`（标签字符串必须自解释，禁空白碎片）、去前后 `_`、
/// 按 char 边界截断 64 字。归空（None）= 该值丢弃。
pub fn normalize_tag_value(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_matches('"').trim();
    // 循环剥句末标点（容忍「。」这类标点+空白组合）
    loop {
        let trimmed = s.trim_end_matches(['。', '.', '！', '!', '？', '?', '；', ';', '，', ',']);
        if trimmed.len() == s.len() {
            break;
        }
        s = trimmed.trim_end();
    }
    let mut out = String::with_capacity(s.len());
    let mut pending_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_ws = true;
            continue;
        }
        if pending_ws && !out.is_empty() {
            out.push('_');
        }
        pending_ws = false;
        out.push(ch);
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        return None;
    }
    let out: String = out.chars().take(TAG_VALUE_MAX_CHARS).collect();
    (!out.is_empty()).then_some(out)
}

/// key-val 拍平（统一标签裁决）：key = 任务名（受控，不自由发挥），val = 离散值；
/// val 已带 `{task_name}-` 前缀则原样返回（重扇出不二次加前缀）。
pub fn flatten_key_value(task_name: &str, value: &str) -> String {
    if value.starts_with(&format!("{task_name}-")) {
        value.to_string()
    } else {
        format!("{task_name}-{value}")
    }
}

/// 写单条标签。`task_id` 仅 custom 来源有值。
///
/// 冲突策略（uq_tag_asset_value）：
/// * manual：**升级**为人工（覆盖 auto/custom/path），`model/taskId` 清空；
/// * auto/custom/path：仅在同来源行上刷新溯源，绝不改写 manual（人工优先）。
pub fn upsert_tag(
    conn: &Connection,
    asset_id: i64,
    value: &str,
    origin: &str,
    model: Option<&str>,
    task_id: Option<i64>,
    score: f64,
) -> Result<bool> {
    let value = value.trim();
    if value.is_empty() {
        return Err(StorageError::Invalid("标签值不能为空".into()));
    }
    let n = if origin == "manual" {
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, model, taskId, score, createdAt)
             VALUES (?1, ?2, 'manual', NULL, NULL, ?3, unixepoch())
             ON CONFLICT(assetId, value) DO UPDATE SET
               origin = 'manual', model = NULL, taskId = NULL, score = ?3
             WHERE tag.origin != 'manual'",
            rusqlite::params![asset_id, value, score],
        )?
    } else {
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, model, taskId, score, createdAt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
             ON CONFLICT(assetId, value) DO UPDATE SET
               model = excluded.model, taskId = excluded.taskId, score = excluded.score
             WHERE tag.origin = excluded.origin",
            rusqlite::params![asset_id, value, origin, model, task_id, score],
        )?
    };
    Ok(n > 0)
}

/// 写结果行（同资产同任务一行，name=维度名快照，value=模型输出原文）。
/// 事务无关：调用方已在事务里时可直接用。
pub fn write_tag_result_row(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    name: &str,
    value: &str,
    model: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tag_result (assetId, taskId, name, value, model, score, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, ?4, ?5, 1.0, unixepoch(), unixepoch())
         ON CONFLICT(assetId, taskId) DO UPDATE SET
           name = excluded.name, value = excluded.value,
           model = excluded.model, updatedAt = unixepoch()",
        rusqlite::params![asset_id, task_id, name.trim(), value.trim(), model],
    )?;
    Ok(())
}

/// 清掉该任务此前产出的标签（只动 origin='custom' 且 taskId 指向本任务的行）。
/// 事务无关。
pub fn clear_custom_tags(conn: &Connection, asset_id: i64, task_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM tag WHERE assetId = ?1 AND taskId = ?2 AND origin = 'custom'",
        rusqlite::params![asset_id, task_id],
    )?)
}

/// 结果 value 扇出为标签（先清旧、再按「;」拆分写入）。事务无关，返回写入条数。
pub fn fanout_custom_tags(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    value: &str,
    model: &str,
) -> Result<usize> {
    clear_custom_tags(conn, asset_id, task_id)?;
    let mut n = 0usize;
    for v in split_values(value) {
        n += conn.execute(
            "INSERT INTO tag (assetId, value, origin, model, taskId, score, createdAt)
             VALUES (?1, ?2, 'custom', ?3, ?4, 1.0, unixepoch())
             ON CONFLICT(assetId, value) DO UPDATE SET
               model = excluded.model, taskId = excluded.taskId, score = 1.0
             WHERE tag.origin = 'custom'",
            rusqlite::params![asset_id, v, model, task_id],
        )?;
    }
    Ok(n)
}

/// 数组扇出（统一标签主路径）：先清该任务旧标签（重跑不残留），再逐值
/// normalize → 拍平 `{task_name}-{val}` → upsert(origin='custom')。返回写入条数。
pub fn fanout_values(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    task_name: &str,
    values: &[String],
    model: &str,
) -> Result<usize> {
    clear_custom_tags(conn, asset_id, task_id)?;
    let mut n = 0usize;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for v in values {
        let Some(v) = normalize_tag_value(v) else {
            continue;
        };
        // 归一化后去重（"青岛 " 与 "青岛" 同值只扇出一次）
        if !seen.insert(v.clone()) {
            continue;
        }
        let v = flatten_key_value(task_name, &v);
        n += conn.execute(
            "INSERT INTO tag (assetId, value, origin, model, taskId, score, createdAt)
             VALUES (?1, ?2, 'custom', ?3, ?4, 1.0, unixepoch())
             ON CONFLICT(assetId, value) DO UPDATE SET
               model = excluded.model, taskId = excluded.taskId, score = 1.0
             WHERE tag.origin = 'custom'",
            rusqlite::params![asset_id, v, model, task_id],
        )?;
    }
    Ok(n)
}

/// 写一条打标任务结果并扇出标签（自定义任务的唯一写入口）。
///
/// 语义：结果行 upsert（同资产同任务一行）；该任务此前产出的标签先删后插，
/// 保证重跑不留旧值、也不会残留已从输出里消失的标签。返回写入的标签条数。
pub fn replace_tag_result(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    name: &str,
    value: &str,
    model: &str,
) -> Result<usize> {
    if split_values(value).is_empty() {
        return clear_tag_result(conn, asset_id, task_id).map(|_| 0);
    }
    let tx = conn.unchecked_transaction()?;
    write_tag_result_row(&tx, asset_id, task_id, name, value, model)?;
    let n = fanout_custom_tags(&tx, asset_id, task_id, value, model)?;
    tx.commit()?;
    Ok(n)
}

/// 数组入口（统一标签主路径）：结果行 value 存模型原文的**数组快照 JSON**，
/// 扇出走 [`fanout_values`]（normalize + key-val 拍平）。全部值归空 →
/// [`clear_tag_result`]（重跑不留旧值）。返回写入的标签条数。
pub fn replace_tag_result_values(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    name: &str,
    values: &[String],
    model: &str,
) -> Result<usize> {
    if values.iter().all(|v| normalize_tag_value(v).is_none()) {
        return clear_tag_result(conn, asset_id, task_id).map(|_| 0);
    }
    let snapshot = serde_json::to_string(values)
        .map_err(|e| StorageError::Invalid(format!("标签数组快照序列化失败: {e}")))?;
    let tx = conn.unchecked_transaction()?;
    write_tag_result_row(&tx, asset_id, task_id, name, &snapshot, model)?;
    let n = fanout_values(&tx, asset_id, task_id, name, values, model)?;
    tx.commit()?;
    Ok(n)
}

/// 清掉某资产某任务的结果行与其产出的标签（模型输出空 → 重跑不留旧值）。
pub fn clear_tag_result(conn: &Connection, asset_id: i64, task_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    clear_custom_tags(&tx, asset_id, task_id)?;
    let n = tx.execute(
        "DELETE FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
        rusqlite::params![asset_id, task_id],
    )?;
    tx.commit()?;
    Ok(n)
}

/// 单资产的全部打标任务结果（维度名: 值）。
pub fn asset_tag_results(conn: &Connection, asset_id: i64) -> Result<Vec<TagResultRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, assetId, taskId, name, value, model, score, createdAt, updatedAt
         FROM tag_result WHERE assetId = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map([asset_id], |r| {
        Ok(TagResultRecord {
            id: r.get(0)?,
            assetId: r.get(1)?,
            taskId: r.get(2)?,
            name: r.get(3)?,
            value: r.get(4)?,
            model: r.get(5)?,
            score: r.get(6)?,
            createdAt: r.get(7)?,
            updatedAt: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrate;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn add_asset(c: &Connection) -> i64 {
        c.execute(
            "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', '/x/a.jpg', 'a.jpg', 'jpg', 1, 0, 0)",
            [],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn split_values_takes_multi_and_dedups() {
        assert_eq!(
            split_values(" 海边日落; 沙滩；海边日落 \n"),
            vec!["海边日落".to_string(), "沙滩".to_string()]
        );
        assert!(split_values("  ;; ").is_empty());
    }

    #[test]
    fn result_fans_out_to_tags_and_rerun_clears_old() {
        let c = db();
        let a = add_asset(&c);
        c.execute(
            "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
             VALUES ('拍摄地点', '在哪拍的', 1, 0, 0)",
            [],
        )
        .unwrap();
        let t: i64 = c.last_insert_rowid();

        let n = replace_tag_result(&c, a, t, "拍摄地点", "海边;沙滩", "vl").unwrap();
        assert_eq!(n, 2);
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(vals, vec!["沙滩".to_string(), "海边".to_string()]);
        // 来源可区分
        let (origin, model, task_id): (String, String, i64) = c
            .query_row(
                "SELECT origin, model, taskId FROM tag WHERE assetId=?1 AND value='海边'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), model.as_str(), task_id), ("custom", "vl", t));
        // 结果表保留原始整串 + 维度名
        let (name, raw) = asset_tag_results(&c, a)
            .unwrap()
            .into_iter()
            .map(|r| (r.name, r.value))
            .next()
            .unwrap();
        assert_eq!((name.as_str(), raw.as_str()), ("拍摄地点", "海边;沙滩"));

        // 重跑输出变化 → 旧标签不残留
        replace_tag_result(&c, a, t, "拍摄地点", "山顶", "vl").unwrap();
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(vals, vec!["山顶".to_string()]);
    }

    #[test]
    fn empty_output_clears_result_and_tags() {
        let c = db();
        let a = add_asset(&c);
        c.execute(
            "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
             VALUES ('地点', 'x', 1, 0, 0)",
            [],
        )
        .unwrap();
        let t = c.last_insert_rowid();
        replace_tag_result(&c, a, t, "地点", "海边", "vl").unwrap();
        assert_eq!(replace_tag_result(&c, a, t, "地点", "  ", "vl").unwrap(), 0);
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM tag WHERE assetId=?1", [a], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        assert!(asset_tag_results(&c, a).unwrap().is_empty());
    }

    #[test]
    fn manual_wins_and_is_never_overwritten() {
        let c = db();
        let a = add_asset(&c);
        c.execute(
            "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
             VALUES ('地点', 'x', 1, 0, 0)",
            [],
        )
        .unwrap();
        let t = c.last_insert_rowid();
        upsert_tag(&c, a, "海边", "manual", None, None, 1.0).unwrap();
        replace_tag_result(&c, a, t, "地点", "海边", "vl").unwrap();
        let (origin, model): (String, Option<String>) = c
            .query_row(
                "SELECT origin, model FROM tag WHERE assetId=?1 AND value='海边'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), model.is_none()), ("manual", true));
        // 结果表仍然记录了模型输出（标签实体是人工的，溯源在结果表）
        assert_eq!(asset_tag_results(&c, a).unwrap().len(), 1);
        // 人工写同值 → 升级为 manual（若此前是 auto）
        let b = add_asset(&c);
        upsert_tag(&c, b, "海边", "auto", Some("m"), None, 0.9).unwrap();
        upsert_tag(&c, b, "海边", "manual", None, None, 1.0).unwrap();
        let origin: String = c
            .query_row(
                "SELECT origin FROM tag WHERE assetId=?1 AND value='海边'",
                [b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(origin, "manual");
    }

    // ---------- 统一标签（t_d65d55a1）：数组输出 + key-val 拍平 + value 索引 ----------

    #[test]
    fn normalize_tag_value_folds_whitespace_and_trims() {
        // 裁决口径：空白折叠成 _、前后 _ 剥掉、引号/句末标点剥掉、64 字截断
        assert_eq!(
            normalize_tag_value("  MacBook   Pro 13寸。 "),
            Some("MacBook_Pro_13寸".to_string())
        );
        assert_eq!(
            normalize_tag_value("海尔 冰箱 501L"),
            Some("海尔_冰箱_501L".to_string())
        );
        assert_eq!(normalize_tag_value("\"暖橙\""), Some("暖橙".to_string()));
        assert_eq!(normalize_tag_value(" size-4k "), Some("size-4k".to_string()));
        // 值内部已有的 _ 保持不动（两级分隔符：- 只做 key-val，_ 只做值内连接）
        assert_eq!(
            normalize_tag_value("MacBook_Pro_13寸"),
            Some("MacBook_Pro_13寸".to_string())
        );
        // 全空白 / 纯标点 → None（该值丢弃）
        assert_eq!(normalize_tag_value("   "), None);
        assert_eq!(normalize_tag_value("。;;"), None);
        assert_eq!(normalize_tag_value("\"  \""), None);
        assert_eq!(normalize_tag_value(""), None);
        // 超长按 char 边界截断，不切多字节
        let long = format!("长{}寸", "很".repeat(TAG_VALUE_MAX_CHARS));
        let out = normalize_tag_value(&long).unwrap();
        assert_eq!(out.chars().count(), TAG_VALUE_MAX_CHARS);
    }

    #[test]
    fn array_replace_fans_out_key_val_and_rerun_clears_old() {
        let c = db();
        let a = add_asset(&c);
        c.execute(
            "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
             VALUES ('拍摄地点', '在哪拍的', 1, 0, 0)",
            [],
        )
        .unwrap();
        let t: i64 = c.last_insert_rowid();

        // 服务端直出数组：值带空白/重复/空值 → 归一化 + 去重 + 拍平成 key-val
        let values = vec![
            "青岛".to_string(),
            "海 边".to_string(),
            "  ".to_string(),
            "青岛 ".to_string(),
        ];
        let n = replace_tag_result_values(&c, a, t, "拍摄地点", &values, "vl").unwrap();
        assert_eq!(n, 2, "空值丢弃、重复归一后去重");
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            vals,
            vec!["拍摄地点-海_边".to_string(), "拍摄地点-青岛".to_string()]
        );
        // 溯源不变：origin='custom' + model/taskId
        let (origin, task): (String, i64) = c
            .query_row(
                "SELECT origin, taskId FROM tag WHERE assetId=?1 AND value='拍摄地点-青岛'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), task), ("custom", t));
        // 结果表存数组快照 JSON（模型原文留档）
        let raw: String = c
            .query_row(
                "SELECT value FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
                [a, t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, r#"["青岛","海 边","  ","青岛 "]"#);

        // 已带 key 前缀的值不二次加前缀（重扇出/模型自己带上都安全）
        let n = replace_tag_result_values(
            &c,
            a,
            t,
            "拍摄地点",
            &["拍摄地点-山顶".to_string()],
            "vl",
        )
        .unwrap();
        assert_eq!(n, 1);
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(vals, vec!["拍摄地点-山顶".to_string()]);

        // 重跑输出空 / 全空白 → 结果行与产出标签一并清掉
        assert_eq!(
            replace_tag_result_values(&c, a, t, "拍摄地点", &[], "vl").unwrap(),
            0
        );
        assert_eq!(
            replace_tag_result_values(&c, a, t, "拍摄地点", &[" ".to_string()], "vl").unwrap(),
            0
        );
        let (tags, results): (i64, i64) = c
            .query_row(
                "SELECT (SELECT COUNT(*) FROM tag WHERE assetId=?1),
                        (SELECT COUNT(*) FROM tag_result WHERE assetId=?1)",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((tags, results), (0, 0));
    }

    #[test]
    fn fanout_values_acceptance_rolling_drum_washer() {
        // 验收场景（t_d65d55a1 ②）：模型数组 ["滚筒_10KG","波轮_8KG"] + 任务名「尺寸」
        // → tag 两行：尺寸-滚筒_10KG、尺寸-波轮_8KG（值内 _ 原样保留，key 用 -）
        let c = db();
        let a = add_asset(&c);
        c.execute(
            "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
             VALUES ('尺寸', '这台电器是什么规格', 1, 0, 0)",
            [],
        )
        .unwrap();
        let t: i64 = c.last_insert_rowid();
        let n = replace_tag_result_values(
            &c,
            a,
            t,
            "尺寸",
            &["滚筒_10KG".to_string(), "波轮_8KG".to_string()],
            "vl",
        )
        .unwrap();
        assert_eq!(n, 2);
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY id")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            vals,
            vec!["尺寸-滚筒_10KG".to_string(), "尺寸-波轮_8KG".to_string()]
        );
    }

    #[test]
    fn tag_value_index_serves_value_lookup() {
        let c = db();
        // 索引在（0023）
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_tag_value'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "idx_tag_value 必须存在");
        // 且真的被 WHERE value=? 查询计划选中（不再全表扫）
        let plan: String = c
            .query_row(
                "EXPLAIN QUERY PLAN SELECT assetId FROM tag WHERE value = 'x'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("idx_tag_value"), "plan={plan}");
        assert!(!plan.contains("SCAN"), "不许全表扫: {plan}");
    }
}
