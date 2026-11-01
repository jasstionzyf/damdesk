//! [`ComplianceRepo`] 的 SQLite 实现（存储解耦裁决：方言写法只允许出现在本模块）。
//!
//! 可移植性说明（PG 并行维护约定）：
//! - 时间戳用 `unixepoch()`（SQLite 3.38+；PG 侧对应 extract(epoch from now())，届时换方言文件）；
//! - schema 类型全部可移植（INTEGER 主键 / TEXT / 整型时间戳，见 migration 0011）；
//! - 「job_id=None 取每 (asset, rule) 最新一行」用 `MAX(id)` 群组子查询，无窗口函数依赖。

use rusqlite::Connection;

use crate::compliance::{
    AssetSnapshot, ComplianceRepo, FeedbackFilter, FeedbackRow, GroupSourceRow, HumanReviewFilter,
    HumanReviewRow, NewFeedback, NewHumanReview, NewVerdict, VerdictFilter, VerdictRow,
};
use crate::{Result, StorageError};

/// SQLite 连接上的合规判定仓库（编排器/导出按 trait 使用，不直接摸连接）。
pub struct SqliteComplianceRepo<'a> {
    conn: &'a Connection,
}

/// 生效判定词的 SQL 投影（0028）：`COALESCE(verdict, result)` 的完整形态。
///
/// `verdict` 有值 = 判定结论；`verdict IS NULL` 时按执行结果回落 ——
/// `skipped` → `skipped`（该规则对这张图没有结论），其余（ok/failed）→ `undetermined`
/// （**判不了 ≠ 通过**；处理类行也没有审核结论）。
macro_rules! effective_verdict {
    ($a:literal) => {
        concat!(
            "COALESCE(",
            $a,
            ".verdict, CASE ",
            $a,
            ".result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END)"
        )
    };
}

/// 判定行的列投影（SELECT 侧统一口径）：生效判定词 + 执行结果 + 审核结论三列必须同时带，
/// 免得各处自己拼（拼错 = 界面把「跑成功」当「通过」）。
macro_rules! verdict_cols {
    () => {
        concat!(
            "id, packId, assetId, jobId, ruleId, ",
            effective_verdict!("rule_verdict"),
            ", result, verdict, confidence, score, evidence, ",
            "fixAction, fixTier, recheckResult, ts, judgementScope, groupKey"
        )
    };
}

pub(crate) const EFFECTIVE_VERDICT_SQL: &str = effective_verdict!("v");

/// 资产**当前内容版本**的 SQL 表达式（DECouple-BE 人审失效条件的唯一实现点）。
///
/// 口径：`asset_versions(current=1).sha256 ?? assets.fileHash ?? ''`
/// —— 修图产出的工作版本带 sha256（0032），原地索引资产带导入 sha256（0001）；
/// 两者都没有（空库/测试）时为空串，与同样空串的人审记录相等（不会把「都没版本」误判成失效）。
///
/// `asset_ref` = 关联资产 id 的列引用（如 `human_review.assetId`）。
pub fn current_content_version_sql(asset_ref: &str) -> String {
    format!(
        "SELECT COALESCE(av.sha256, a.fileHash, '') FROM assets a \
         LEFT JOIN asset_versions av ON av.assetId = a.assetId AND av.current = 1 \
         WHERE a.assetId = {asset_ref}"
    )
}

/// 读单个资产的当前内容版本（写人审时把当时版本快照落库）。
///
/// 资产不存在 → 空串（不报错：写人审前先查资产存在性是调用方的事，这里只负责版本口径）。
pub fn asset_content_version(conn: &Connection, asset_id: i64) -> Result<String> {
    let sql = current_content_version_sql("?1");
    match conn.query_row(&sql, rusqlite::params![asset_id], |r| r.get::<_, String>(0)) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

impl<'a> SqliteComplianceRepo<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

impl ComplianceRepo for SqliteComplianceRepo<'_> {
    fn insert_verdict(&self, v: &NewVerdict) -> Result<i64> {
        // 结果行契约（0028 + DEC-F2/t_a4b7e0ee/0037）：判定词拆成 result(执行结果) + verdict(审核结论)
        // 两列；skipped 行除 verdict 外**连 confidence/score 一并落 NULL**——
        // 「没有结论」不携带分数与把握度（NULL，不是 0，不是 1.0）。
        // 非法判定词由 verdict 的 CHECK 拒收（verdict_reject 会把原值写进错误信息）。
        self.conn
            .execute(
                "INSERT INTO rule_verdict
                     (packId, assetId, jobId, ruleId, result, verdict, confidence, score, evidence,
                      fixAction, fixTier, recheckResult, ts, judgementScope, groupKey)
                 VALUES (?1, ?2, ?3, ?4,
                         CASE WHEN ?5 = 'skipped' THEN 'skipped' ELSE 'ok' END,
                         CASE WHEN ?5 = 'skipped' THEN NULL ELSE ?5 END,
                         CASE WHEN ?5 = 'skipped' THEN NULL ELSE ?6 END,
                         CASE WHEN ?5 = 'skipped' THEN NULL ELSE ?7 END,
                         ?8, ?9, ?10, ?11, unixepoch(), ?12, ?13)",
                rusqlite::params![
                    v.pack_id,
                    v.asset_id,
                    v.job_id,
                    v.rule_id,
                    v.result,
                    v.confidence,
                    v.score,
                    v.evidence,
                    v.fix_action,
                    v.fix_tier,
                    v.recheck_result,
                    v.judgement_scope,
                    v.group_key,
                ],
            )
            .map_err(|e| verdict_reject(v, e))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// IP-K05：插件来源列补写（migration 0027）。只在插件通道调用。
    fn set_verdict_pack_source(
        &self,
        verdict_id: i64,
        pack_version: &str,
        plugin_hash: &str,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE rule_verdict SET packVersion = ?2, pluginHash = ?3 WHERE id = ?1",
            rusqlite::params![verdict_id, pack_version, plugin_hash],
        )?)
    }

    fn delete_verdicts(&self, pack_id: &str, job_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM rule_verdict WHERE packId = ?1 AND jobId = ?2",
            rusqlite::params![pack_id, job_id],
        )?)
    }

    fn find_verdicts(&self, f: &VerdictFilter) -> Result<Vec<VerdictRow>> {
        // job_id = Some → 该批次全部判定；run_id = Some → 该次执行（N 个批次行）的全部判定；
        // 都 None → 每 (asset, rule) 最新一行（跨批次重跑后报告取最新状态；MAX(id) 单调递增主键，可移植 SQL）
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(f.pack_id.clone())];
        let mut sql = format!("SELECT {} FROM rule_verdict WHERE ", verdict_cols!());
        if let Some(job) = f.job_id {
            params.push(Box::new(job));
            sql.push_str("packId = ?1 AND jobId = ?2");
        } else if let Some(run) = f.run_id {
            // DECouple-BE：一次执行 = compliance_runs.runId，其下 task 全部带同一个 job_queue.runId
            params.push(Box::new(run));
            sql.push_str(
                "packId = ?1 AND jobId IN (SELECT id FROM job_queue WHERE runId = ?2)",
            );
        } else {
            sql.push_str(
                "id IN (SELECT MAX(id) FROM rule_verdict WHERE packId = ?1 GROUP BY assetId, ruleId)",
            );
        }
        if let Some(asset_id) = f.asset_id {
            let idx = params.len() + 1;
            params.push(Box::new(asset_id));
            sql.push_str(&format!(" AND assetId = ?{idx}"));
        }
        sql.push_str(" ORDER BY assetId, ruleId, id");
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), map_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// C9 组键解析（`field = None` → folder 源：取 absPath 父目录）。
    ///
    /// 列名**必须**在白名单内（core-types::GROUP_ASSET_FIELDS）——白名单外直接报错，
    /// 因为列名要拼进 SQL（唯一允许的动态片段，别处一律参数化）。
    fn group_source_rows(
        &self,
        asset_ids: &[i64],
        field: Option<&str>,
    ) -> Result<Vec<GroupSourceRow>> {
        if asset_ids.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(f) = field {
            if !core_types::group_asset_field_ok(f) {
                return Err(StorageError::Invalid(format!(
                    "group_by 组键列 {f:?} 不在白名单（已知: {}）",
                    core_types::group_asset_fields_joined()
                )));
            }
        }
        // 动态列名只可能是白名单里的常量；?1..?n 为 assetId 列表
        let value_expr = match field {
            Some(f) => format!("a.{f}"),
            None => "NULL".to_string(),
        };
        let placeholders = (1..=asset_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT a.assetId, a.absPath, {value_expr} FROM assets a
             WHERE a.assetId IN ({placeholders}) ORDER BY a.assetId"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            asset_ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            let asset_id: i64 = r.get(0)?;
            let abs_path: String = r.get(1)?;
            let raw: Option<String> = r.get(2)?;
            // 空串/纯空白 = 缺键（不把空格当组名）；folder 源用父目录
            let value = match field {
                Some(_) => raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                None => std::path::Path::new(&abs_path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .filter(|s| !s.is_empty()),
            };
            Ok(GroupSourceRow {
                asset_id,
                abs_path,
                value,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn find_group_verdicts(
        &self,
        pack_id: &str,
        rule_id: &str,
        group_key: &str,
    ) -> Result<Vec<VerdictRow>> {
        let mut stmt = self.conn.prepare(concat!(
            "SELECT ",
            verdict_cols!(),
            " FROM rule_verdict
             WHERE packId = ?1 AND ruleId = ?2 AND groupKey = ?3
             ORDER BY ts, id"
        ))?;
        let rows = stmt.query_map(rusqlite::params![pack_id, rule_id, group_key], map_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn find_asset_verdicts(&self, pack_id: &str, asset_id: i64) -> Result<Vec<VerdictRow>> {
        // 每 (ruleId, judgementScope) 取最新一行：逐图结论与组级结论各自成为「最新」，
        // 互不遮蔽（占位行 skipped 与组级真结论同 (asset,rule) 但不同 scope）。
        let mut stmt = self.conn.prepare(concat!(
            "SELECT ",
            verdict_cols!(),
            " FROM rule_verdict
             WHERE id IN (SELECT MAX(id) FROM rule_verdict
                          WHERE packId = ?1 AND assetId = ?2
                          GROUP BY ruleId, COALESCE(judgementScope, ''))
             ORDER BY ruleId, COALESCE(judgementScope, ''), id"
        ))?;
        let rows = stmt.query_map(rusqlite::params![pack_id, asset_id], map_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn asset_snapshot(&self, asset_id: i64) -> Result<Option<AssetSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.assetId, a.absPath, a.mediaType, a.missing, a.deleted, r.width, r.height,
                    a.fileHash, r.pHash
             FROM assets a
             LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
             WHERE a.assetId = ?1",
        )?;
        let mut rows = stmt.query_map([asset_id], |r| {
            Ok(AssetSnapshot {
                asset_id: r.get(0)?,
                abs_path: r.get(1)?,
                media_type: r.get(2)?,
                missing: r.get(3)?,
                deleted: r.get(4)?,
                width: r.get(5)?,
                height: r.get(6)?,
                file_hash: r.get(7)?,
                p_hash: r.get(8)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    fn insert_feedback(&self, f: &NewFeedback) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO rule_feedback
                     (packId, assetId, ruleId, machineResult, humanResult, reason, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())",
                rusqlite::params![
                    f.pack_id,
                    f.asset_id,
                    f.rule_id,
                    f.machine_result,
                    f.human_result,
                    f.reason,
                ],
            )
            .map_err(|e| feedback_reject(f, e))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn find_feedback(&self, f: &FeedbackFilter) -> Result<Vec<FeedbackRow>> {
        // 逐条件拼接（assetId / ruleId / limit 都可选）；占位符编号随条件推进。
        let mut clauses = vec!["packId = ?1".to_string()];
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(f.pack_id.clone())];
        if let Some(a) = f.asset_id {
            clauses.push(format!("assetId = ?{}", params.len() + 1));
            params.push(Box::new(a));
        }
        if let Some(r) = &f.rule_id {
            clauses.push(format!("ruleId = ?{}", params.len() + 1));
            params.push(Box::new(r.clone()));
        }
        let mut sql = format!(
            "SELECT id, packId, assetId, ruleId, machineResult, humanResult, reason, ts
             FROM rule_feedback WHERE {} ORDER BY ts, id",
            clauses.join(" AND ")
        );
        if let Some(l) = f.limit {
            sql.push_str(&format!(" LIMIT {}", l.max(0)));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(FeedbackRow {
                id: r.get(0)?,
                pack_id: r.get(1)?,
                asset_id: r.get(2)?,
                rule_id: r.get(3)?,
                machine_result: r.get(4)?,
                human_result: r.get(5)?,
                reason: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn count_feedback(&self, pack_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM rule_feedback WHERE packId = ?1",
            [pack_id],
            |r| r.get(0),
        )?)
    }

    // ---------- COMPLIANCE-HUMAN：人工改判（0035 `human_review`） ----------

    fn save_human_review(&self, rev: &NewHumanReview) -> Result<i64> {
        // 覆盖语义 + 历史保留：先落戳旧活记录（同 (asset, scheme, rule|资产级)），再插新行。
        // 事务内完成 —— 中途失败不留「无活记录」的中间态（部分唯一索引也只保证不双活）。
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE human_review
                SET revokedAt = unixepoch(), revokeReason = 'superseded'
              WHERE schemeId = ?1 AND assetId = ?2 AND ruleId IS ?3 AND revokedAt IS NULL",
            rusqlite::params![rev.scheme_id, rev.asset_id, rev.rule_id],
        )?;
        tx.execute(
            "INSERT INTO human_review
                 (assetId, schemeId, ruleId, judgementScope, groupKey, humanStatus, humanReason,
                  judgedAt, reviewedBy, contentVersion, machineResult, machineConfidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch(), ?8, ?9, ?10, ?11)",
            rusqlite::params![
                rev.asset_id,
                rev.scheme_id,
                rev.rule_id,
                rev.judgement_scope,
                rev.group_key,
                rev.human_status,
                rev.human_reason,
                rev.reviewed_by,
                rev.content_version,
                rev.machine_result,
                rev.machine_confidence,
            ],
        )
        .map_err(|e| review_reject(rev, e))?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn revoke_human_review(
        &self,
        scheme_id: &str,
        asset_id: i64,
        rule_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<usize> {
        // 撤销 = 活记录落戳（不删行：改判历史要留着可追）
        Ok(self.conn.execute(
            "UPDATE human_review
                SET revokedAt = unixepoch(), revokeReason = ?4
              WHERE schemeId = ?1 AND assetId = ?2 AND ruleId IS ?3 AND revokedAt IS NULL",
            rusqlite::params![scheme_id, asset_id, rule_id, reason],
        )?)
    }

    fn find_human_reviews(&self, f: &HumanReviewFilter) -> Result<Vec<HumanReviewRow>> {
        let mut clauses = vec!["schemeId = ?1".to_string()];
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(f.scheme_id.clone())];
        if let Some(a) = f.asset_id {
            clauses.push(format!("assetId = ?{}", params.len() + 1));
            params.push(Box::new(a));
        }
        match &f.rule_id {
            None => {}
            // Some(None) = 资产级；Some(Some(id)) = 具体规则
            Some(None) => clauses.push("ruleId IS NULL".to_string()),
            Some(Some(r)) => {
                clauses.push(format!("ruleId = ?{}", params.len() + 1));
                params.push(Box::new(r.clone()));
            }
        }
        if f.active_only {
            clauses.push("revokedAt IS NULL".to_string());
        }
        // 失效条件：只认「改判时的内容版本 == 资产当前内容版本」的记录。
        // 资产修图（新工作版本 sha256）或重导换内容（新 fileHash）后，旧人审自动不再生效。
        if f.current_content_only {
            clauses.push(format!(
                "contentVersion = ({})",
                current_content_version_sql("human_review.assetId")
            ));
        } else if f.only_stale_content {
            clauses.push(format!(
                "contentVersion <> ({})",
                current_content_version_sql("human_review.assetId")
            ));
        }
        let mut sql = format!(
            "SELECT id, schemeId, assetId, ruleId, judgementScope, groupKey, humanStatus, humanReason,
                    judgedAt, reviewedBy, contentVersion, machineResult, machineConfidence,
                    revokedAt, revokeReason
             FROM human_review WHERE {} ORDER BY judgedAt, id",
            clauses.join(" AND ")
        );
        if let Some(l) = f.limit {
            sql.push_str(&format!(" LIMIT {}", l.max(0)));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(HumanReviewRow {
                id: r.get(0)?,
                scheme_id: r.get(1)?,
                asset_id: r.get(2)?,
                rule_id: r.get(3)?,
                judgement_scope: r.get(4)?,
                group_key: r.get(5)?,
                human_status: r.get(6)?,
                human_reason: r.get(7)?,
                human_at: r.get(8)?,
                reviewed_by: r.get(9)?,
                content_version: r.get(10)?,
                machine_result: r.get(11)?,
                machine_confidence: r.get(12)?,
                revoked_at: r.get(13)?,
                revoke_reason: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VerdictRow> {
    Ok(VerdictRow {
        id: r.get(0)?,
        pack_id: r.get(1)?,
        asset_id: r.get(2)?,
        job_id: r.get(3)?,
        rule_id: r.get(4)?,
        // 生效判定词（投影列）→ 执行结果 → 审核结论
        result: r.get(5)?,
        exec_result: r.get(6)?,
        verdict: r.get(7)?,
        confidence: r.get(8)?,
        score: r.get(9)?,
        evidence: r.get(10)?,
        fix_action: r.get(11)?,
        fix_tier: r.get(12)?,
        recheck_result: r.get(13)?,
        ts: r.get(14)?,
        judgement_scope: r.get(15)?,
        group_key: r.get(16)?,
    })
}

// ---------- C6：库视图「合规状态」筛选的 SQL 口径 ----------

/// 合规状态 → SQL 布尔条件（对 assets 表别名 `a`），供库视图 WHERE 拼接。
///
/// 返回 (表达式, 参数值)；占位符从 `next_index` 开始编号，packId **复用同一编号**多次出现
/// （SQLite 允许重复引用同一编号参数）。调用方把返回的参数按序 push 进自己的参数向量。
///
/// 与 `svc_library::compliance_view::state_of_verdicts_with`（Rust 口径）必须逐字同义：
/// 该语义由单测 `svc_library::compliance_view::tests::sql_and_rust_agree_on_every_state`
/// 在覆盖三态 + 人工改判（规则级 / 资产级 / 撤销后）的夹具上逐资产比对
/// （SQL 取出的 id 集合 == Rust 算出的 id 集合）。
///
/// COMPLIANCE-HUMAN（0017）：结论一律取**生效结论** `humanStatus ?? result` ——
/// 人工改判活记录按 (asset, rule) 左连（部分唯一索引保证一行最多一条，不会扇出），
/// 资产级改判（`ruleId IS NULL`）整张图直接取人工裁决。两者都必须参与，否则
/// 「合规页说已改判、库筛选还按机器结论筛」两处口径就漂移了。
///
/// ⚠ 方言：本函数用了 SQLite **JSON1**（json_valid/json_extract）读 evidence.on_fail
/// （软 fail 判定）。PG 化时这里必须平行实现（jsonb `->>` 同语义），三层不能用同一段 SQL。
/// 三值逻辑坑（本函数第一版就踩了）：`NULL = 'pass'` 结果是 NULL 而不是 false，
/// 会让"evidence 没有 on_fail 键"的行被整条过滤掉 → 硬 fail 变消失。故一律 COALESCE 兜底。
pub fn state_condition_sql(
    filter: &core_types::ComplianceStateFilter,
    next_index: usize,
) -> (String, Vec<String>) {
    let p = next_index;
    // 最新判定行集合（每 asset × rule × 作用域 一行）：C10 起组级结论按成员展开落库，
    // 与逐图判定同 (assetId, ruleId) 但作用域不同 —— 不把作用域放进分组，两者会互相遮蔽
    // （谁后写谁说话），库视图筛选就与 Rust 口径（state_of_verdicts）漂移。
    let latest = format!(
        "(SELECT MAX(id) FROM rule_verdict WHERE packId = ?{p} \
         GROUP BY assetId, ruleId, COALESCE(judgementScope, 'per_asset'))"
    );
    // 生效结论 = humanStatus ?? result（人工改判活记录按 (asset, rule) 左连）。
    // DECouple-BE：0035 `human_review` 的键是 (assetId, schemeId, ruleId)，且带**失效条件** ——
    // 只有「改判时的内容版本 == 资产当前内容版本」的活记录才参与口径（资产修图/重导后旧人审失效）。
    // 这里必须与 `HumanReviewFilter::current_content_only` 的 SQL 完全同义，否则
    // 「Rust 口径 vs SQL 口径」对账用例会红（单测 sql_and_rust_agree_on_every_state 守这条）。
    let live_review = format!(
        "h.schemeId = 'pack:' || ?{p} AND h.assetId = v.assetId AND h.ruleId = v.ruleId \
         AND h.revokedAt IS NULL AND h.contentVersion = ({cur})",
        cur = current_content_version_sql("h.assetId")
    );
    let hit = |extra: &str| {
        format!(
            "EXISTS (SELECT 1 FROM rule_verdict v \
             LEFT JOIN human_review h ON {live_review} \
             WHERE v.packId = ?{p} AND v.assetId = a.assetId AND v.id IN {latest}{extra})"
        )
    };
    // 硬 fail = 生效结论是 fail 且不是软 fail（evidence.on_fail = pass 的打标型/软规则）；
    // 人工改判成 fail 的一律算硬 fail（人的裁决没有软硬之分）。
    // N5b（0028）：结论列从 `result` 换成 `verdict`（可空），生效判定词走统一投影 ——
    // 拼错一处就会出现「库里 result='ok' 而界面当通过」的漂移。
    let hard_fail = hit(&format!(
        " AND COALESCE(h.humanStatus, {EFFECTIVE_VERDICT_SQL}) = 'fail' AND (h.id IS NOT NULL \
         OR {EFFECTIVE_VERDICT_SQL} <> 'fail' \
         OR COALESCE(CASE WHEN COALESCE(json_valid(v.evidence), 0) = 1 \
         THEN json_extract(v.evidence, '$.on_fail') END, '') <> 'pass')"
    ));
    // 三态：undetermined = 判定过程出错（图片读不出/VL 超时或不可解析/插件异常），
    // 是「有判定行但没有结论」，与 unjudged（一行判定都没有）不是一回事。
    let undetermined = hit(&format!(
        " AND COALESCE(h.humanStatus, {EFFECTIVE_VERDICT_SQL}) = 'undetermined'"
    ));
    let judged = hit("");
    // 「有结论」= 至少一行生效结论是 pass/fail。全 skipped（= 每条规则对这张图都不适用）
    // 不算判过：与 Rust 侧 `state_of_verdicts_with` 同步（卡 t_9267ed7a）——
    // 先前这种资产落 pass 分支，把「没判」算成「合格」并进了合格统计。
    let concluded = hit(&format!(
        " AND {EFFECTIVE_VERDICT_SQL} IN ('pass', 'fail')"
    ));
    // 资产级改判（ruleId IS NULL）：整张图的结论直接取人工裁决
    let asset_review = |extra: &str| {
        format!(
            "EXISTS (SELECT 1 FROM human_review h WHERE h.schemeId = 'pack:' || ?{p} \
             AND h.assetId = a.assetId AND h.ruleId IS NULL AND h.revokedAt IS NULL \
             AND h.contentVersion = ({cur}){extra})",
            cur = current_content_version_sql("h.assetId")
        )
    };
    let ov_any = asset_review("");
    let ov_fail = asset_review(" AND h.humanStatus = 'fail'");
    let ov_pass = asset_review(" AND h.humanStatus = 'pass'");
    let sql = match filter.state.as_str() {
        // 汇总口径与 state_of_verdicts_with 逐条对齐（每个状态都先要求 judged：
        // 没跑过判定的资产不受资产级改判影响 —— 改判必须建立在机器判定之上，接口层也拒收）：
        //   fail         资产级改判 fail，或（无资产级改判且）任一硬 fail（压过 undetermined）
        //   undetermined 无资产级改判、无硬 fail、且（有 undetermined 行 或 **一行结论都没有**）
        //   pass         资产级改判 pass，或（无资产级改判、无硬 fail、无 undetermined、**有结论**）
        //   unjudged     一行判定都没有
        "fail" => format!("{judged} AND (({ov_fail}) OR (NOT {ov_any} AND {hard_fail}))"),
        "undetermined" => format!(
            "{judged} AND NOT {ov_any} AND NOT {hard_fail} AND ({undetermined} OR NOT {concluded})"
        ),
        "pass" => format!(
            "{judged} AND (({ov_pass}) OR (NOT {ov_any} AND NOT {hard_fail} \
             AND NOT {undetermined} AND {concluded}))"
        ),
        "unjudged" => format!("NOT {judged}"),
        // 未知状态词 → 空集（HTTP 层已 400；这里不静默当"不过滤"）
        _ => "0".to_string(),
    };
    (sql, vec![filter.packId.clone()])
}

/// CHECK 约束拒收（非法 result/fixTier 等）→ 带上下文的 Invalid，UI 可直接展示。
fn verdict_reject(v: &NewVerdict, e: rusqlite::Error) -> StorageError {
    let constraint = e.to_string();
    if constraint.contains("CHECK") {
        return StorageError::Invalid(format!(
            "rule_verdict 拒收：asset={} rule={} result={:?} fixTier={:?}: {constraint}",
            v.asset_id, v.rule_id, v.result, v.fix_tier
        ));
    }
    StorageError::Sqlite(e)
}

/// 反馈行 CHECK 拒收（machineResult/humanResult 非法值）→ 带上下文的 Invalid。
fn feedback_reject(f: &NewFeedback, e: rusqlite::Error) -> StorageError {
    let constraint = e.to_string();
    if constraint.contains("CHECK") {
        return StorageError::Invalid(format!(
            "rule_feedback 拒收：asset={} rule={} machineResult={:?} humanResult={:?}: {constraint}",
            f.asset_id, f.rule_id, f.machine_result, f.human_result
        ));
    }
    StorageError::Sqlite(e)
}

/// 人工改判行 CHECK 拒收（humanStatus 非法 / reason 空白 / confidence 越界 / 活记录冲突）
/// → 带上下文的 Invalid（接口层能把「为什么没落库」原样展示）。
fn review_reject(r: &NewHumanReview, e: rusqlite::Error) -> StorageError {
    let constraint = e.to_string();
    if constraint.contains("CHECK") || constraint.contains("UNIQUE") {
        return StorageError::Invalid(format!(
            "human_review 拒收：asset={} rule={:?} humanStatus={:?} reason.len={}: {constraint}",
            r.asset_id,
            r.rule_id,
            r.human_status,
            r.human_reason.trim().chars().count()
        ));
    }
    StorageError::Sqlite(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&c).unwrap();
        c
    }

    fn asset(c: &Connection, path: &str) -> i64 {
        c.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (NULL, ?1, 'image', 'a.jpg', 'jpg', 1, 0, 0)",
            rusqlite::params![path],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn with_resource(c: &Connection, asset_id: i64, w: i64, h: i64) {
        c.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
            rusqlite::params![asset_id, w, h],
        )
        .unwrap();
    }

    fn verdict(asset_id: i64, job_id: i64, rule: &str, result: &str) -> NewVerdict {
        NewVerdict {
            pack_id: "mini".into(),
            asset_id,
            job_id,
            rule_id: rule.into(),
            result: result.into(),
            confidence: 0.8,
            score: Some(0.42),
            evidence: Some("{\"k\":1}".into()),
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        }
    }

    #[test]
    fn insert_splits_judgement_word_into_result_and_verdict() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        for (rule, word) in [
            ("R_pass", "pass"),
            ("R_fail", "fail"),
            ("R_undet", "undetermined"),
            ("R_skip", "skipped"),
        ] {
            repo.insert_verdict(&verdict(a, 21, rule, word)).unwrap();
        }
        // 库里的原始两列（0028 口径）：判定词拆开，skipped 不冒充结论
        let raw = |rule: &str| -> (String, Option<String>) {
            c.query_row(
                "SELECT result, verdict FROM rule_verdict WHERE ruleId = ?1",
                [rule],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(raw("R_pass"), ("ok".into(), Some("pass".into())));
        assert_eq!(raw("R_undet"), ("ok".into(), Some("undetermined".into())));
        assert_eq!(raw("R_skip"), ("skipped".into(), None));
        // 读路径投影：老调用方按判定词读 `result`，语义与迁移前逐字一致
        let rows = repo.find_verdicts(&VerdictFilter::job("mini", 21)).unwrap();
        let by_rule = |rule: &str| rows.iter().find(|r| r.rule_id == rule).cloned().unwrap();
        assert_eq!(by_rule("R_pass").result, "pass");
        assert_eq!(by_rule("R_fail").result, "fail");
        assert_eq!(by_rule("R_skip").result, "skipped");
        assert_eq!(by_rule("R_pass").exec_result, "ok");
        assert_eq!(by_rule("R_pass").verdict.as_deref(), Some("pass"));
        assert_eq!(by_rule("R_skip").verdict, None);
    }

    #[test]
    fn exec_failure_without_verdict_reads_as_undetermined() {
        // 处理类行 result='failed'（没有审核结论）：生效判定词必须是 undetermined ——
        // 「跑失败」不等于「通过」，也不等于「不合格」
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        c.execute(
            "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, ts)
             VALUES ('mini', ?1, 31, 'R_batch', 'failed', NULL, 1.0, 0)",
            [a],
        )
        .unwrap();
        let rows = repo.find_verdicts(&VerdictFilter::job("mini", 31)).unwrap();
        assert_eq!(rows[0].result, "undetermined");
        assert_eq!(rows[0].exec_result, "failed");
        assert_eq!(rows[0].verdict, None);
    }

    #[test]
    fn insert_find_delete_roundtrip() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        repo.insert_verdict(&verdict(a, 11, "R1_size", "pass")).unwrap();
        repo.insert_verdict(&verdict(a, 11, "R2_white", "fail")).unwrap();
        // 按 job 查：全量、稳定序
        let rows = repo.find_verdicts(&VerdictFilter::job("mini", 11)).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rule_id, "R1_size");
        assert_eq!(rows[0].result, "pass");
        assert_eq!(rows[0].score, Some(0.42));
        // 其他 pack/job 不串
        assert!(repo.find_verdicts(&VerdictFilter::job("other", 11)).unwrap().is_empty());
        assert!(repo.find_verdicts(&VerdictFilter::job("mini", 12)).unwrap().is_empty());
        // 删除只清本 job
        assert_eq!(repo.delete_verdicts("mini", 11).unwrap(), 2);
        assert!(repo.find_verdicts(&VerdictFilter::pack("mini")).unwrap().is_empty());
    }

    #[test]
    fn find_without_job_takes_latest_per_asset_rule() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        // 两个批次同 (asset, rule)：job 12 是重跑
        repo.insert_verdict(&verdict(a, 11, "R1_size", "fail")).unwrap();
        repo.insert_verdict(&verdict(a, 12, "R1_size", "pass")).unwrap();
        repo.insert_verdict(&verdict(a, 11, "R2_white", "pass")).unwrap();
        let rows = repo.find_verdicts(&VerdictFilter::pack("mini")).unwrap();
        assert_eq!(rows.len(), 2, "每 (asset, rule) 只留最新一行");
        assert_eq!(rows[0].rule_id, "R1_size");
        assert_eq!(rows[0].job_id, Some(12), "取最新 job 的判定");
        assert_eq!(rows[0].result, "pass");
        assert_eq!(rows[1].rule_id, "R2_white");
        assert_eq!(rows[1].job_id, Some(11));
    }

    #[test]
    fn check_constraint_surfaces_as_invalid() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        let err = repo
            .insert_verdict(&verdict(a, 11, "R1", "maybe"))
            .unwrap_err();
        assert!(err.to_string().contains("拒收"), "got {err}");
    }

    // ---------- C9：组级判定（groupKey / group_source_rows / 组内成员可查） ----------

    fn group_verdict(asset_id: i64, job_id: i64, rule: &str, key: &str) -> NewVerdict {
        NewVerdict {
            judgement_scope: Some("per_group".into()),
            group_key: Some(key.into()),
            ..verdict(asset_id, job_id, rule, "pass")
        }
    }

    #[test]
    fn group_verdict_roundtrip_and_members_query() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/prop/g1/a.jpg");
        let b = asset(&c, "/prop/g1/b.jpg");
        let other = asset(&c, "/prop/g2/c.jpg");
        // 同组同规则两条（成员展开）→ 「组内成员可查」
        repo.insert_verdict(&group_verdict(a, 21, "F3_dup", "/prop/g1"))
            .unwrap();
        repo.insert_verdict(&group_verdict(b, 21, "F3_dup", "/prop/g1"))
            .unwrap();
        repo.insert_verdict(&group_verdict(other, 21, "F3_dup", "/prop/g2"))
            .unwrap();
        // per_asset 行不含 groupKey → 不会被组内查询捞到
        repo.insert_verdict(&verdict(a, 22, "F1_sharpness", "pass")).unwrap();

        let rows = repo
            .find_group_verdicts("mini", "F3_dup", "/prop/g1")
            .unwrap();
        assert_eq!(rows.len(), 2, "本组成员两条");
        assert!(rows.iter().all(|r| r.asset_id != other));
        assert!(rows.iter().all(|r| r.judgement_scope.as_deref() == Some("per_group")));
        assert!(rows.iter().all(|r| r.group_key.as_deref() == Some("/prop/g1")));
        // per_asset 行 groupKey = NULL
        let pa = repo.find_verdicts(&VerdictFilter::pack("mini")).unwrap();
        let f1 = pa.iter().find(|r| r.rule_id == "F1_sharpness").unwrap();
        assert_eq!(f1.group_key, None);
        assert_eq!(f1.judgement_scope.as_deref(), Some("per_asset"));
    }

    #[test]
    fn judgement_scope_check_rejects_unknown_word() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        let mut v = verdict(a, 11, "R1", "pass");
        v.judgement_scope = Some("per_team".into());
        let err = repo.insert_verdict(&v).unwrap_err();
        assert!(err.to_string().contains("拒收"), "got {err}");
    }

    #[test]
    fn group_source_rows_folder_and_allowlist() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/prop/g1/a.jpg");
        let b = asset(&c, "/prop/g1/b.jpg");
        let d = asset(&c, "/prop/g2/c.jpg");
        // 列值不填 → None（缺键）；填 "" / "  " → 也算缺键
        c.execute(
            "UPDATE assets SET cameraModel = 'X-T5' WHERE assetId = ?1",
            rusqlite::params![a],
        )
        .unwrap();
        c.execute(
            "UPDATE assets SET cameraModel = '  ' WHERE assetId = ?1",
            rusqlite::params![b],
        )
        .unwrap();

        let folder = repo.group_source_rows(&[a, b, d], None).unwrap();
        assert_eq!(folder.len(), 3);
        assert_eq!(folder[0].value.as_deref(), Some("/prop/g1"));
        assert_eq!(folder[2].value.as_deref(), Some("/prop/g2"), "按 assetId 升序");

        let by_col = repo.group_source_rows(&[a, b, d], Some("cameraModel")).unwrap();
        assert_eq!(by_col[0].value.as_deref(), Some("X-T5"));
        assert_eq!(by_col[1].value, None, "纯空白 = 缺键");
        assert_eq!(by_col[2].value, None, "NULL = 缺键");

        // 白名单外列名：响亮报错，不静默拼 SQL
        let err = repo
            .group_source_rows(&[a], Some("absPath; DROP TABLE assets"))
            .unwrap_err();
        assert!(err.to_string().contains("白名单"), "got {err}");
        assert!(
            repo.group_source_rows(&[], None).unwrap().is_empty(),
            "空输入不查库"
        );
    }

    #[test]
    fn asset_snapshot_joins_primary_resource() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        with_resource(&c, a, 1000, 800);
        let snap = repo.asset_snapshot(a).unwrap().unwrap();
        assert_eq!(snap.abs_path, "/x/a.jpg");
        assert_eq!((snap.width, snap.height), (Some(1000), Some(800)));
        assert_eq!((snap.missing, snap.deleted), (0, 0));
        assert!(repo.asset_snapshot(999).unwrap().is_none());
    }

    // ---------- C6：rule_feedback ----------

    fn feedback(asset_id: i64, rule: &str, machine: &str, human: &str) -> NewFeedback {
        NewFeedback {
            pack_id: "mini".into(),
            asset_id,
            rule_id: rule.into(),
            machine_result: machine.into(),
            human_result: human.into(),
            reason: Some("人工复核".into()),
        }
    }

    #[test]
    fn feedback_insert_find_count() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        let b = asset(&c, "/x/b.jpg");
        repo.insert_feedback(&feedback(a, "R1_size", "fail", "pass")).unwrap();
        repo.insert_feedback(&feedback(a, "R2_white", "undetermined", "pass")).unwrap();
        repo.insert_feedback(&feedback(b, "R1_size", "pass", "fail")).unwrap();
        // 其他 pack 不串
        let mut other = feedback(a, "R1_size", "fail", "fail");
        other.pack_id = "other".into();
        repo.insert_feedback(&other).unwrap();

        assert_eq!(repo.count_feedback("mini").unwrap(), 3);
        assert_eq!(repo.count_feedback("other").unwrap(), 1);

        let all = repo.find_feedback(&FeedbackFilter::pack("mini")).unwrap();
        assert_eq!(all.len(), 3);
        // ts 升序 + id 稳定序（同秒多条不抖）
        assert_eq!(all[0].asset_id, a);
        assert_eq!(all[0].rule_id, "R1_size");
        assert_eq!(all[2].asset_id, b);

        // 收窄到单资产
        let one = repo.find_feedback(&FeedbackFilter::asset("mini", a)).unwrap();
        assert_eq!(one.len(), 2);
        // rule 收窄 + limit
        let mut f = FeedbackFilter::asset("mini", a);
        f.rule_id = Some("R2_white".into());
        f.limit = Some(1);
        let narrowed = repo.find_feedback(&f).unwrap();
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].machine_result, "undetermined");
        assert_eq!(narrowed[0].reason.as_deref(), Some("人工复核"));
        assert_eq!(narrowed[0].human_result, "pass");
    }

    #[test]
    fn feedback_misjudged_semantics() {
        // 机器 pass/fail ≠ 人工结论 = 误判；undetermined 不算误判（机器本来没下结论）
        let base = feedback(1, "R1", "fail", "pass");
        let row = FeedbackRow {
            id: 1,
            pack_id: base.pack_id.clone(),
            asset_id: base.asset_id,
            rule_id: base.rule_id.clone(),
            machine_result: base.machine_result.clone(),
            human_result: base.human_result.clone(),
            reason: base.reason.clone(),
            ts: 0,
        };
        assert!(row.is_misjudged(), "机器 fail / 人工 pass = 误判");
        let agree = FeedbackRow {
            machine_result: "fail".into(),
            human_result: "fail".into(),
            ..row.clone()
        };
        assert!(!agree.is_misjudged(), "结论一致不是误判");
        let from_human_queue = FeedbackRow {
            machine_result: "pending_human".into(),
            human_result: "fail".into(),
            ..row.clone()
        };
        assert!(!from_human_queue.is_misjudged(), "转人工处置不算误判");
    }

    #[test]
    fn feedback_check_constraint_surfaces_as_invalid() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        // 非法 humanResult（schema CHECK：pass|fail）
        let err = repo
            .insert_feedback(&feedback(a, "R1", "fail", "maybe"))
            .unwrap_err();
        assert!(err.to_string().contains("rule_feedback 拒收"), "got {err}");
        // 非法 machineResult
        let err = repo
            .insert_feedback(&feedback(a, "R1", "skipped", "fail"))
            .unwrap_err();
        assert!(err.to_string().contains("rule_feedback 拒收"), "got {err}");
        assert_eq!(repo.count_feedback("mini").unwrap(), 0, "拒收的行不入库");
    }

    #[test]
    fn feedback_asset_cascade_and_empty_pack() {
        let c = db();
        let repo = SqliteComplianceRepo::new(&c);
        let a = asset(&c, "/x/a.jpg");
        repo.insert_feedback(&feedback(a, "R1", "fail", "pass")).unwrap();
        assert!(repo.find_feedback(&FeedbackFilter::pack("nope")).unwrap().is_empty());
        // 资产删除级联（ON DELETE CASCADE）
        c.execute("DELETE FROM assets WHERE assetId = ?1", [a]).unwrap();
        assert_eq!(repo.count_feedback("mini").unwrap(), 0);
    }
}
