//! D10 — 自定义打标任务存储（L0 DAO，PLAN.md 裁决 2026-10-13）。
//!
//! 职责边界：任务 CRUD、`job_queue`(kind='custom_tag') 入队、任务结果落库。
//! 批量执行/重试/断点续跑的队列语义在 L2 `svc-pipeline::custom_queue`
//! （复用 D4 骨架）；本模块被 svc-import（新导入自动入队）与 svc-pipeline 共用，
//! kind/payload 常量以此处为单一事实源。
//!
//! TAG-VALUE（2026-10-19 裁决）后的落库语义：
//! * `custom_tag_task.name` = **维度名**（用户起的，库内唯一），不是标签本身；
//! * 模型输出写 `tag_result`（name=维度名快照、value=输出原文），见 [`crate::tags`]；
//! * value 非空 → 自动扇出为 `tag` 行（origin='custom'，只存标签值），
//!   与人工（manual）/模板打标（auto）/目录（path）来源可区分、人工优先。

use core_types::CustomTagTask;
use rusqlite::Connection;

use crate::{Result, StorageError};

/// job_queue.kind 常量（svc-import 自动入队 / svc-pipeline 消费共用，勿散抄字符串）。
pub const CUSTOM_TAG_KIND: &str = "custom_tag";

/// 单个 job 的 payload JSON（单键 taskId；入队与消费两侧字符串精确匹配，格式冻结）。
pub fn custom_payload(task_id: i64) -> String {
    format!("{{\"taskId\":{task_id}}}")
}

/// 从 job payload 解析 taskId（坏 payload → None，消费侧按跳过处理）。
pub fn parse_custom_payload(payload: Option<&str>) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(payload?).ok()?;
    v.get("taskId").and_then(|x| x.as_i64())
}

fn validate(name: &str, prompt: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StorageError::Invalid("任务名不能为空".into()));
    }
    if name.len() > 64 {
        return Err(StorageError::Invalid("任务名过长（≤64 字符）".into()));
    }
    if prompt.trim().is_empty() {
        return Err(StorageError::Invalid("提示词不能为空".into()));
    }
    Ok(())
}

/// 建任务（name 库内唯一，重复报错）。
pub fn create_task(conn: &Connection, name: &str, prompt: &str, enabled: bool) -> Result<i64> {
    validate(name, prompt)?;
    if name_exists(conn, name.trim(), None)? {
        return Err(StorageError::Invalid(format!(
            "任务名已存在：{}",
            name.trim()
        )));
    }
    conn.execute(
        "INSERT INTO custom_tag_task (name, prompt, enabled, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, unixepoch(), unixepoch())",
        rusqlite::params![name.trim(), prompt.trim(), enabled as i64],
    )?;
    Ok(conn.last_insert_rowid())
}

fn name_exists(conn: &Connection, name: &str, exclude_id: Option<i64>) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM custom_tag_task WHERE name = ?1 AND id != ?2",
        rusqlite::params![name, exclude_id.unwrap_or(-1)],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// 改任务（改名时历史 custom 标签联动改名；与人工同名行冲突则保留人工，跳过该行）。
pub fn update_task(
    conn: &Connection,
    id: i64,
    name: &str,
    prompt: &str,
    enabled: bool,
) -> Result<()> {
    validate(name, prompt)?;
    let name = name.trim();
    if name_exists(conn, name, Some(id))? {
        return Err(StorageError::Invalid(format!("任务名已存在：{name}")));
    }
    let tx = conn.unchecked_transaction()?;
    let old_name: String = tx
        .query_row(
            "SELECT name FROM custom_tag_task WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| StorageError::Invalid(format!("任务不存在：id={id}")))?;
    tx.execute(
        "UPDATE custom_tag_task SET name = ?1, prompt = ?2, enabled = ?3, updatedAt = unixepoch()
         WHERE id = ?4",
        rusqlite::params![name, prompt.trim(), enabled as i64, id],
    )?;
    if old_name != name {
        // 维度名只影响结果表（tag 表没有 name 列，标签值不受任务改名影响）
        tx.execute(
            "UPDATE tag_result SET name = ?1 WHERE taskId = ?2",
            rusqlite::params![name, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 删任务。purge=false（默认）保留历史标签与结果（仅断溯源，FK 置 NULL）；
/// purge=true 同时清理该任务的 custom 标签行与结果行（UI 二次确认后用）。
/// 注意顺序：先清标签/结果再删任务行——FK `ON DELETE SET NULL` 会先把 taskId
/// 置空，后清就匹配不到了。返回 (删除任务数, 清理标签数)。
pub fn delete_task(conn: &Connection, id: i64, purge: bool) -> Result<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let n_tag = if purge {
        let n = tx.execute(
            "DELETE FROM tag WHERE taskId = ?1 AND origin = 'custom'",
            [id],
        )?;
        tx.execute("DELETE FROM tag_result WHERE taskId = ?1", [id])?;
        n
    } else {
        0
    };
    let n_task = tx.execute("DELETE FROM custom_tag_task WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok((n_task, n_tag))
}

fn row_to_task(conn: &Connection, r: &rusqlite::Row) -> rusqlite::Result<CustomTagTask> {
    let id: i64 = r.get("id")?;
    Ok(CustomTagTask {
        id,
        name: r.get("name")?,
        prompt: r.get("prompt")?,
        enabled: r.get::<_, i64>("enabled")? == 1,
        createdAt: r.get("createdAt")?,
        updatedAt: r.get("updatedAt")?,
        jobPending: job_count(conn, id, "('pending','running')"),
        jobDone: job_count(conn, id, "('done')"),
        jobFailed: job_count(conn, id, "('failed')"),
        lastRunAt: conn
            .query_row(
                "SELECT MAX(updatedAt) FROM job_queue
                 WHERE kind = ?1 AND payload = ?2 AND status IN ('done','failed')",
                rusqlite::params![CUSTOM_TAG_KIND, custom_payload(id)],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten(),
    })
}

fn job_count(conn: &Connection, task_id: i64, statuses: &str) -> i64 {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM job_queue
             WHERE kind = ?1 AND payload = ?2 AND status IN {statuses}"
        ),
        rusqlite::params![CUSTOM_TAG_KIND, custom_payload(task_id)],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// 任务列表（含每任务队列计数与最近执行时间）。
pub fn list_tasks(conn: &Connection) -> Result<Vec<CustomTagTask>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, prompt, enabled, createdAt, updatedAt FROM custom_tag_task
                      ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| row_to_task(conn, r))?;
    let mut out = Vec::new();
    for t in rows {
        out.push(t?);
    }
    Ok(out)
}

/// 单任务（不存在 → None；消费侧跳过用）。
pub fn get_task(conn: &Connection, id: i64) -> Result<Option<CustomTagTask>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, prompt, enabled, createdAt, updatedAt FROM custom_tag_task WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map([id], |r| row_to_task(conn, r))?;
    Ok(rows.next().transpose()?)
}

/// 全量（重）跑入队：清空该任务 job 行后对全部可打标资产入队
/// （重跑语义；tag 层 upsert 保证不重复插）。返回入队数。
pub fn enqueue_task_all(conn: &Connection, task_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let payload = custom_payload(task_id);
    tx.execute(
        "DELETE FROM job_queue WHERE kind = ?1 AND payload = ?2",
        rusqlite::params![CUSTOM_TAG_KIND, payload],
    )?;
    let n = tx.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT ?1, assetId, ?2, 'pending', unixepoch(), unixepoch()
         FROM assets a
         WHERE a.deleted = 0 AND a.missing = 0 AND a.mediaType = 'image'
           AND EXISTS (SELECT 1 FROM custom_tag_task t WHERE t.id = ?3 AND t.enabled = 1)",
        rusqlite::params![CUSTOM_TAG_KIND, payload, task_id],
    )?;
    tx.commit()?;
    Ok(n)
}

/// 单资产 × 全部 enabled 任务入队（导入/watcher 钩子；NOT EXISTS 幂等）。
/// 任务不存在或全部停用时零写入。返回入队数。
pub fn enqueue_for_asset(conn: &Connection, asset_id: i64) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT id FROM custom_tag_task WHERE enabled = 1")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut n = 0;
    let mut ins = conn.prepare(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT ?1, ?2, ?3, 'pending', unixepoch(), unixepoch()
         WHERE NOT EXISTS (
             SELECT 1 FROM job_queue j
             WHERE j.kind = ?1 AND j.targetId = ?2 AND j.payload = ?3
               AND j.status != 'done'
         )",
    )?;
    for id in ids {
        n += ins.execute(rusqlite::params![
            CUSTOM_TAG_KIND,
            asset_id,
            custom_payload(id)
        ])?;
    }
    Ok(n)
}

/// 失败 job 重置为 pending（UI「重试失败项」）。
pub fn retry_failed(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL, updatedAt = unixepoch()
         WHERE kind = ?1 AND status = 'failed'",
        [CUSTOM_TAG_KIND],
    )?)
}

/// 打标任务结果落库：写 `tag_result`（维度名 + 输出原文）并把非空 value 扇出到
/// `tag`（origin='custom'，按「;」拆成多个标签，人工同值行不被覆盖）。
/// 模型输出空 → 清掉该资产该任务的结果行与其产出标签（重跑不留旧值）。
/// 返回写入的标签条数。
pub fn upsert_custom_tag(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    name: &str,
    value: &str,
    model: &str,
) -> Result<usize> {
    crate::tags::replace_tag_result(conn, asset_id, task_id, name, value, model)
}

/// 数组入口（统一标签主路径）：结果行 value 存数组快照 JSON，扇出 normalize +
/// key-val 拍平（`{任务名}-{值}`）；全空数组 → 清结果与旧标签。见
/// [`crate::tags::replace_tag_result_values`]。返回写入的标签条数。
pub fn upsert_custom_tag_values(
    conn: &Connection,
    asset_id: i64,
    task_id: i64,
    name: &str,
    values: &[String],
    model: &str,
) -> Result<usize> {
    crate::tags::replace_tag_result_values(conn, asset_id, task_id, name, values, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&c).unwrap();
        c
    }

    fn add_asset(c: &Connection, path: &str) -> i64 {
        c.execute(
            "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', ?1, 'a.jpg', 'jpg', 1, 0, 0)",
            rusqlite::params![path],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn task_row(c: &Connection, id: i64) -> (String, String, i64) {
        c.query_row(
            "SELECT name, prompt, enabled FROM custom_tag_task WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn crud_roundtrip_and_unique_name() {
        let c = db();
        let t1 = create_task(&c, "拍摄地点", "这张照片的拍摄地点是什么？", true).unwrap();
        let t2 = create_task(&c, "色调氛围", "描述主色调与氛围", false).unwrap();
        assert_ne!(t1, t2);

        // 重名（含前后空白归一后）必须报错
        assert!(create_task(&c, " 拍摄地点 ", "x", true).is_err());
        assert!(create_task(&c, "", "x", true).is_err());
        assert!(create_task(&c, "n", "  ", true).is_err());

        let tasks = list_tasks(&c).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "拍摄地点");
        assert!(tasks[0].enabled);
        assert_eq!(tasks[1].name, "色调氛围");
        assert!(!tasks[1].enabled);

        // 改名只影响结果表的维度名；标签值（tag.value）不受任务改名影响
        let a = add_asset(&c, "/x/a.jpg");
        upsert_custom_tag(&c, a, t1, "拍摄地点", "海边日落", "vl").unwrap();
        update_task(&c, t1, "改名后", "新提示词", true).unwrap();
        assert_eq!(task_row(&c, t1).0, "改名后");
        let (rn, rv): (String, String) = c
            .query_row(
                "SELECT name, value FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
                rusqlite::params![a, t1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rn.as_str(), rv.as_str()), ("改名后", "海边日落"));
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(vals, vec!["海边日落".to_string()]);

        // 改名撞已存在任务名 → 报错
        assert!(update_task(&c, t1, "色调氛围", "p", true).is_err());

        // 删除：默认保留标签与结果（仅断溯源）；purge 一并清理
        let (n, tags) = delete_task(&c, t2, false).unwrap();
        assert_eq!((n, tags), (1, 0));
        let kept: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE origin = 'custom'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
        let (n, tags) = delete_task(&c, t1, true).unwrap();
        assert_eq!((n, tags), (1, 1));
        let total: i64 = c
            .query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 0);
        let results: i64 = c
            .query_row("SELECT COUNT(*) FROM tag_result", [], |r| r.get(0))
            .unwrap();
        assert_eq!(results, 0);
        assert!(get_task(&c, t1).unwrap().is_none());
    }

    #[test]
    fn custom_result_fanout_and_manual_priority() {
        let c = db();
        let a = add_asset(&c, "/x/a.jpg");
        let t = create_task(&c, "色调", "主色调", true).unwrap();

        // 重跑覆盖：结果表一行、标签跟着输出走（不留旧值）
        assert_eq!(
            upsert_custom_tag(&c, a, t, "色调", "暖橙", "m1").unwrap(),
            1
        );
        assert_eq!(
            upsert_custom_tag(&c, a, t, "色调", "冷蓝", "m2").unwrap(),
            1
        );
        let (v, m, tid, score): (String, String, i64, f64) = c
            .query_row(
                "SELECT value, model, taskId, score FROM tag WHERE assetId = ?1",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((v.as_str(), m.as_str(), tid), ("冷蓝", "m2", t));
        assert!((score - 1.0).abs() < 1e-9);

        // 人工同值优先：custom 不覆盖 manual 行，但其余值照常追加
        let b = add_asset(&c, "/x/b.jpg");
        c.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (?1, '冷蓝', 'manual', 0)",
            [b],
        )
        .unwrap();
        assert_eq!(
            upsert_custom_tag(&c, b, t, "色调", "冷蓝;暖橙", "m").unwrap(),
            1
        );
        let rows: Vec<(String, String)> = c
            .prepare("SELECT origin, value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([b], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("manual".to_string(), "冷蓝".to_string()),
                ("custom".to_string(), "暖橙".to_string()),
            ]
        );
        // 结果表保留原始整串 + 维度名
        let raw: String = c
            .query_row(
                "SELECT value FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
                rusqlite::params![b, t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "冷蓝;暖橙");
    }

    #[test]
    fn migration_0021_tag_value_only_schema() {
        let c = db();
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('tag')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.iter().any(|x| x == "value"), "cols={cols:?}");
        assert!(
            !cols.iter().any(|x| x == "name"),
            "tag 不应再有 name 列: {cols:?}"
        );
        assert!(cols.iter().any(|x| x == "taskId"), "cols={cols:?}");
        // 同图同值唯一
        let a = add_asset(&c, "/x/a.jpg");
        c.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (?1, '狗', 'manual', 0)",
            [a],
        )
        .unwrap();
        assert!(c
            .execute(
                "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (?1, '狗', 'auto', 0)",
                [a],
            )
            .is_err());
        // 结果表存在（name=维度名 + value=输出）
        c.execute(
            "INSERT INTO tag_result (assetId, name, value, createdAt, updatedAt)
             VALUES (?1, '地点', '海边', 0, 0)",
            [a],
        )
        .unwrap();
    }

    #[test]
    fn enqueue_scopes_and_idempotence() {
        let c = db();
        let t_on = create_task(&c, "地点", "p", true).unwrap();
        let t_off = create_task(&c, "氛围", "p", false).unwrap();
        let a1 = add_asset(&c, "/x/1.jpg");
        add_asset(&c, "/x/2.jpg");

        // 新导入自动入队：仅 enabled 任务、幂等（未终态 job 不重复入队）
        assert_eq!(enqueue_for_asset(&c, a1).unwrap(), 1);
        assert_eq!(enqueue_for_asset(&c, a1).unwrap(), 0);
        c.execute(
            "UPDATE job_queue SET status='done' WHERE kind = ?1 AND targetId = ?2",
            rusqlite::params![CUSTOM_TAG_KIND, a1],
        )
        .unwrap();
        assert_eq!(enqueue_for_asset(&c, a1).unwrap(), 1); // done 后可重新入队（重跑）
        assert_eq!(enqueue_for_asset(&c, a1).unwrap(), 0);

        // 全量（重）跑：清空该任务 job 行后重建全部可打标资产；disabled 任务 0
        assert_eq!(enqueue_task_all(&c, t_on).unwrap(), 2);
        assert_eq!(enqueue_task_all(&c, t_off).unwrap(), 0);

        // payload 冻结格式 & 计数读数
        assert_eq!(custom_payload(t_on), format!("{{\"taskId\":{t_on}}}"));
        assert_eq!(
            parse_custom_payload(Some(&custom_payload(t_on))),
            Some(t_on)
        );
        assert_eq!(parse_custom_payload(None), None);
        assert_eq!(parse_custom_payload(Some("garbage")), None);
        let tasks = list_tasks(&c).unwrap();
        let on = tasks.iter().find(|t| t.id == t_on).unwrap();
        assert_eq!(on.jobPending, 2);
        let off = tasks.iter().find(|t| t.id == t_off).unwrap();
        assert_eq!(off.jobPending, 0);

        // 失败重置
        c.execute(
            "UPDATE job_queue SET status='failed' WHERE kind = ?1",
            [CUSTOM_TAG_KIND],
        )
        .unwrap();
        assert_eq!(retry_failed(&c).unwrap(), 2);
    }
}
