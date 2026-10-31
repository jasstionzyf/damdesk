//! SQLite 版本化迁移：`schema_migrations` 表记录已应用版本，逐版本事务执行。
//! 迁移纪律（PLAN.md §3）：只加不改不删。

use rusqlite::Connection;

use crate::{Result, StorageError};

/// (version, name, sql) — 按版本升序声明，SQL 内嵌编译。
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "init_core_tables",
        include_str!("migrations/0001_init.sql"),
    ),
    (
        2,
        "import_batches",
        include_str!("migrations/0002_import_batches.sql"),
    ),
    (
        3,
        "seed_settings",
        include_str!("migrations/0003_seed_settings.sql"),
    ),
    (
        4,
        "inference_embed_state",
        include_str!("migrations/0004_inference_embed_state.sql"),
    ),
    (
        5,
        "dedup_vectors",
        include_str!("migrations/0005_dedup_vectors.sql"),
    ),
    (
        6,
        "path_tag_origin",
        include_str!("migrations/0006_path_tag_origin.sql"),
    ),
    (
        7,
        "custom_tag_tasks",
        include_str!("migrations/0007_custom_tag_tasks.sql"),
    ),
    (
        8,
        "smart_folders",
        include_str!("migrations/0008_smart_folders.sql"),
    ),
    (
        9,
        "tag_review",
        include_str!("migrations/0009_tag_review.sql"),
    ),
    (
        10,
        "import_auto_tag",
        include_str!("migrations/0010_import_auto_tag.sql"),
    ),
    (
        11,
        "rule_verdict_feedback",
        include_str!("migrations/0011_rule_verdict_feedback.sql"),
    ),
    (12, "ocr_text", include_str!("migrations/0012_ocr_text.sql")),
    // EXIF2（task）：元数据派生列 + 画幅 + GPS（本卡此前 0012 被 OCR1 占用 → 用 0013）
    (
        13,
        "exif_meta",
        include_str!("migrations/0013_exif_meta.sql"),
    ),
    // OCR-MATCH（task）：OCR 字面检索 FTS5 倒排（中文 bigram 预切分 + unicode61）
    // 合流裁决（task）：本卡原为 0013，main 已被 EXIF2 占用 → 顺延 0014（只改编号，SQL 不动）
    (14, "ocr_fts", include_str!("migrations/0014_ocr_fts.sql")),
    // C9（task）：组级判定的落点（groupKey + judgementScope）。
    // 卡里写的 0012 已被 OCR1 占用 → 顺延 0014；合流（task）再顺延 0015（OCR-MATCH 先占 0014）。
    (
        15,
        "group_judgement",
        include_str!("migrations/0015_group_judgement.sql"),
    ),
    // COMPLIANCE-TRISTATE（task）：判定结果收敛三态 + confidence。
    // 需要重建 rule_verdict（0011 的 CHECK 不能改），行/id 全保留，旧值按映射搬迁。
    (
        16,
        "tristate_verdict",
        include_str!("migrations/0016_tristate_verdict.sql"),
    ),
    // COMPLIANCE-HUMAN（task）：人工二次审核（改判 + reason 必填 + 撤销），
    // 与机器判定分表（rule_verdict 永不改写），活记录部分唯一索引防双活。
    (
        17,
        "human_review",
        include_str!("migrations/0017_human_review.sql"),
    ),
    // CMP-JOB-01（task）：一次批量审核 = 一个 run（`compliance_runs`）+ task 挂 runId。
    // 用户裁决：各次执行的结果统计必须分开，历史累计数字不得和本次并排误导。
    (
        18,
        "compliance_runs",
        include_str!("migrations/0018_compliance_runs.sql"),
    ),
    // KI-2c（task）：pHash D4 八变体 —— 新增 `pHashes` TEXT（JSON 数组，
    // 老 `pHash` 列原位保留为 identity 变体，旧读路径不受影响）；老库单值回填为
    // `["<hex>"]`（只补 NULL 行 → 幂等）。合并时编号顺延：0018 已被 compliance_runs 占用。
    (
        19,
        "phash_d4_variants",
        include_str!("migrations/0019_phash_d4.sql"),
    ),
    // KI-2b（task）：向量判重的边表 + 补判账本。判重塔向量落库后写 `dup_edges`，
    // 分组时与 fileHash/pHash 判据并查集合并（分层展示、kind 分开标注）。
    // 编号避让：合并后落 0020（0018 compliance_runs / 0019 pHash D4 已占）。
    (
        20,
        "dup_edges",
        include_str!("migrations/0020_dup_edges.sql"),
    ),
    // TAG-VALUE（2026-10-19 裁决）：tag 只存标签值（无 name），打标任务结果落
    // `tag_result`（name=维度名 / value=模型输出），非空结果自动扇出到 tag。
    // 换表搬迁 + 按「;」拆分既有 custom 值，语义等价物（列名 name→value）。
    (
        21,
        "tag_value_only",
        include_str!("migrations/0021_tag_value_only.sql"),
    ),
    // MIGRATE-COPYIN（task）：参考目录（reference）资产搬到库根的明细账本。
    // 卡里写的 0021 已被 TAG-VALUE 占用 → 顺延 0022（只改编号，语义不变）。
    (
        22,
        "migrate_audit",
        include_str!("migrations/0022_migrate_audit.sql"),
    ),
    // UNIFIED-TAG-A（task）：统一标签后过滤/统计只按 tag.value 走，
    // uq(assetId, value) 前导列不是 value → WHERE value=? / GROUP BY value 全表扫，补前导索引。
    (
        23,
        "tag_value_index",
        include_str!("migrations/0023_tag_value_index.sql"),
    ),
    (
        24,
        "tag_value_nocase_index",
        include_str!("migrations/0024_tag_value_nocase_index.sql"),
    ),
    (
        25,
        "tag_merge_log",
        include_str!("migrations/0025_tag_merge_log.sql"),
    ),
    // IP-K05（task）：行业包身份账本 + 判定行的插件来源列。
    // 卡面写的是 0025/0026，但 0025 已被标签审计（task）占用 → 顺延 0026/0027（只改编号，
    // SQL 与语义不动；合流纪律见 damdesk-repo「迁移编号并发撞车」）。
    (
        26,
        "packs_registry",
        include_str!("migrations/0026_packs_registry.sql"),
    ),
    (
        27,
        "verdict_pack_source",
        include_str!("migrations/0027_verdict_pack_source.sql"),
    ),
    // N5b（task）：结果行契约落库 —— rule_verdict.result 拆成 result(执行结果) +
    // verdict(审核结论，可空) + 插件安装记录 kinds。
    (
        28,
        "result_verdict_contract",
        include_str!("migrations/0028_result_verdict_contract.sql"),
    ),
    // N1 规格实体（卡 task）：插件下的「规格」= 选规则 + 阈值 + 动作链 + 输出 + 参数 diff。
    // 规格是用户资产：卸插件不删规格，迁移幂等靠 `originKey` 唯一索引。
    // 合流编号（N链整合）：本卡原占 0028，被 N5b 结果契约占用 → 顺延 0029（只改编号/SQL 文件名，语义不动）。
    (
        29,
        "plugin_spec",
        include_str!("migrations/0029_plugin_spec.sql"),
    ),
    // IP-K16（task）：插件唯一化 —— 判定/反馈行的 packId 统一到插件身份 id、
    // 清掉「激活行业包 / 已导入行业包 / 合规引擎」设置项。
    // 合流重编号（N18/task）：K16 与 N5b 各自占了 28（N5b 已落真实库），K16 顺延 30
    // （只改编号，SQL 与语义不动；账本按 name 判重，旧库不会重复执行）。
    (
        30,
        "plugin_unify",
        include_str!("migrations/0030_plugin_unify.sql"),
    ),
    // N18（task）：旧配置迁移收尾 —— 规格里还写着「配置包 id」的 pluginId 改键到插件身份
    // （K16 删了配置包之后那些 id 不再可装载，不改就是一条永远跑不了死规格）。
    (
        31,
        "spec_plugin_ids",
        include_str!("migrations/0031_spec_plugin_ids.sql"),
    ),
    // N6e（task）：宿主侧动作链的产物账本（一资产至多一张工作版本）。
    // 合流编号：原 0029 被 N1 规格占用 → 顺延 0032（排在 N18 的 0030/0031 之后，只改编号与文件名）。
    (
        32,
        "asset_versions",
        include_str!("migrations/0032_asset_versions.sql"),
    ),
    // DM-A4（task，KI-2 残项）：资产级导入批次溯源 —— `assets.batchId`。
    // 去重 `range=batch:<id>` 从此按「这一次导入事件导进来的行」精确过滤，不再拿
    // `import_batches.rootPath` 当目录前缀猜（同目录两次导入会串批）。
    // 历史行保持 NULL（不回填、不编造来源）；老库回退见 app_web::dedup_api 的 approx 分支。
    (
        33,
        "asset_batch_id",
        include_str!("migrations/0033_asset_batch_id.sql"),
    ),
    // DM-C1（task）：OCR 文本行框落库 —— inpaint 重绘区域要与「这里有文本」的本地实测
    // 证据交叉验证（VL bbox 不再无脑当区域）。
    // 合流编号（DM-CLOSE/task）：本卡原定 0033 被 DM-A4 的 asset_batch_id 占用且先合流，
    // 故顺延为 0034（只改编号与文件名，SQL 与语义不动；账本按 name 判重，旧库不会重复执行）。
    (
        34,
        "ocr_boxes",
        include_str!("migrations/0034_ocr_boxes.sql"),
    ),
    // DECouple-BE（task，用户裁决 2026-10-31）：人审独立成表 `human_review` ——
    // 键从 `(packId, assetId, ruleId)` 改为 `(assetId, schemeId, ruleId)`，并带**内容版本失效条件**
    // （`contentVersion`：改判时的资产内容版本；读取只认与当前内容版本相等的活记录）。
    // 存量 `verdict_human_review` 行在同一迁移内幂等搬迁（legacyId 唯一索引兜底），旧表由
    // `scripts/migrate_human_review_v2.py` 在回读校验 100% 后 purge（不在启动迁移里 DROP）。
    (
        35,
        "human_review_v2",
        include_str!("migrations/0035_human_review_v2.sql"),
    ),
];

/// 建迁移账本表（幂等）。
fn ensure_ledger(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

/// 已应用的迁移版本号（测试对账用，仅测试编译）。
#[cfg(test)]
fn applied_versions(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 账本全量（version, name）—— 判重既要版本号也要名字，见 `migrate` 的重编号说明。
fn applied_entries(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT version, name FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 跑完全部未应用迁移。
///
/// **判重口径 = 名字优先**（2026-10-17 真机事故驱动）：合流时迁移会被重新编号 —— C9 的
/// `group_judgement` 在卡分支上是 0014，合流后 OCR-MATCH 先占 0014、它顺延成 0015。用旧
/// 包建出来的库，账本里 `14=group_judgement` 且列已在位；只按版本号判重就会重放 0015 →
/// `duplicate column name: judgementScope` → 桌面端 boot panic（整机起不来的级别）。
/// 所以：**同名即已应用**，跳过；版本号只在名字对不上时用来避让（顺延到账本最大值之后），
/// 干净库的编号行为与之前完全一致。
pub fn migrate(conn: &Connection) -> Result<()> {
    migrate_upto(conn, i64::MAX)
}

/// 只跑到 `max_version` 为止（含）。
///
/// 存在理由：**迁移前后对比**要造「迁移前形态的真实旧库」——先用它把库建到 0027，
/// 灌历史判定行，再跑 [`migrate`] 应用 0028，逐行比对结论值不变。
/// 生产路径只有 [`migrate`]（跑到最新），这个函数不得被业务代码调用。
pub fn migrate_upto(conn: &Connection, max_version: i64) -> Result<()> {
    ensure_ledger(conn)?;
    let mut applied = applied_entries(conn)?;
    for (version, name, sql) in MIGRATIONS {
        if *version > max_version {
            continue;
        }
        if applied.iter().any(|(_, n)| n == name) {
            continue;
        }
        // 声明的版本号已被别的迁移占用（重编号场景）→ 顺延，避免 schema_migrations 主键冲突
        let target = if applied.iter().any(|(v, _)| v == version) {
            applied.iter().map(|(v, _)| *v).max().unwrap_or(0) + 1
        } else {
            *version
        };
        let tx = conn.unchecked_transaction()?;
        if let Err(e) = tx.execute_batch(sql) {
            return Err(StorageError::Migration {
                version: *version,
                name: (*name).to_string(),
                msg: e.to_string(),
            });
        }
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, unixepoch())",
            rusqlite::params![target, name],
        )?;
        tx.commit()?;
        applied.push((target, (*name).to_string()));
    }
    Ok(())
}

/// 当前 schema 版本（测试/诊断用）。
pub fn current_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT IFNULL(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_in_memory_twice_ok() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等
                                 // 断言绑到迁移表本身（新增迁移不必改测试，且不会漏断言）
        let top = MIGRATIONS.iter().map(|(v, _, _)| *v).max().unwrap();
        assert_eq!(current_version(&conn).unwrap(), top);
        assert_eq!(applied_versions(&conn).unwrap().len(), MIGRATIONS.len());
        // D3：0004 推理状态列必须在位（emb_status/emb_model）
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('assets')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.iter().any(|c| c == "emb_status"), "cols={cols:?}");
        assert!(cols.iter().any(|c| c == "emb_model"), "cols={cols:?}");
        // D5：0005 去重状态列 + 向量表登记簿必须在位
        assert!(cols.iter().any(|c| c == "dedup_status"), "cols={cols:?}");
        assert!(cols.iter().any(|c| c == "dedup_model"), "cols={cols:?}");
        let vc = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='vector_tables'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(vc, 1, "vector_tables registry must exist");
        // KI-2b：0019 向量判重边表 + 补判账本必须在位（关系表，不是临时算的）
        assert!(
            cols_contains(&conn, "dup_edges", "score"),
            "dup_edges 表缺列"
        );
        assert!(cols_contains(&conn, "dup_judge_state", "value"));
        // D8：origin='path' 必须可入库（0006 重建 tag 表扩展 CHECK）
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/a/x.jpg', 'x.jpg', 'jpg', 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '旅行', 'path', unixepoch())",
            [],
        )
        .unwrap();
        let origins: Vec<String> = conn
            .prepare("SELECT DISTINCT origin FROM tag ORDER BY origin")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(origins.iter().any(|o| o == "path"), "origins={origins:?}");
        // 重建后唯一索引仍在（同值不同 origin 不能成两行）
        assert!(conn
            .execute(
                "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '旅行', 'auto', unixepoch())",
                [],
            )
            .is_err());
        // D10：0007 自定义打标任务表 + tag 扩展列 + origin='custom' 放宽
        assert!(cols_contains(&conn, "custom_tag_task", "name"));
        let tag_cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('tag')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(tag_cols.iter().any(|c| c == "value"), "{tag_cols:?}");
        assert!(tag_cols.iter().any(|c| c == "taskId"), "{tag_cols:?}");
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '自定义标签值', 'custom', unixepoch())",
            [],
        )
        .unwrap();
        // TAG-VALUE（0021）：tag 只有 value；任务结果表在（name=维度名 / value=输出）
        assert!(
            !tag_cols.iter().any(|c| c == "name"),
            "tag 不应再有 name 列: {tag_cols:?}"
        );
        assert!(cols_contains(&conn, "tag_result", "name"));
        assert!(cols_contains(&conn, "tag_result", "value"));
        // C0：0011 合规判定两表在位 + packId+ruleId 归属（换 pack 历史判定可查）
        assert!(cols_contains(&conn, "rule_verdict", "packId"));
        assert!(cols_contains(&conn, "rule_verdict", "recheckResult"));
        assert!(cols_contains(&conn, "rule_feedback", "humanResult"));
        conn.execute(
            "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, score, evidence, fixAction, fixTier, recheckResult, ts)
             VALUES ('ecommerce-taobao', 1, 7, 'R4_subject_ratio', 'ok', 'fail', 0.62, 0.42, '{\"fg_ratio\":0.42}', 'smart_crop', 'guaranteed', 'pass', unixepoch())",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rule_verdict (packId, assetId, ruleId, result, verdict, confidence, ts)
             VALUES ('realestate', 1, 'R1_size', 'ok', 'undetermined', 0.0, unixepoch())",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rule_feedback (packId, assetId, ruleId, machineResult, humanResult, reason, ts)
             VALUES ('ecommerce-taobao', 1, 'R3_no_watermark', 'fail', 'pass', '角落小 logo 非促销文案', unixepoch())",
            [],
        )
        .unwrap();
        // CHECK 约束拒收非法 result
        assert!(conn
            .execute(
                "INSERT INTO rule_verdict (packId, assetId, ruleId, result, ts)
                 VALUES ('p', 1, 'R1', 'maybe', unixepoch())",
                [],
            )
            .is_err());
        // 判定历史按 packId+ruleId 归属：同 ruleId 不同 pack 各自可查（换包不丢）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rule_verdict WHERE packId='ecommerce-taobao' AND ruleId='R4_subject_ratio'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let packs: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT packId) FROM rule_verdict WHERE ruleId='R1_size'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(packs, 1);
    }

    /// 头版本随最新迁移走：合并多张卡后编号会顺延，断言绑到最新版本而不是写死数字。
    fn top_version() -> i64 {
        *MIGRATIONS.iter().map(|(v, _, _)| v).max().unwrap()
    }

    fn cols_contains(conn: &Connection, table: &str, col: &str) -> bool {
        conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .any(|c| c == col)
    }

    /// 回归：**合流重编号**的旧库必须能升级（2026-10-17 真机事故）。
    ///
    /// 旧包（C9 卡分支 wt/task）把 `group_judgement` 编到 0014；合流后 OCR-MATCH 先占
    /// 0014、它顺延成 0015。旧库账本 `14=group_judgement` 且两列已在位，按版本号判重会重放
    /// 0015 → `duplicate column name: judgementScope` → 桌面端 boot panic。
    #[test]
    fn migrate_tolerates_renumbered_ledger() {
        let conn = Connection::open_in_memory().unwrap();
        // 造旧库：按顺序跑到 0013，再把 group_judgement 的 SQL 记为 14（旧编号、无 ocr_fts）
        ensure_ledger(&conn).unwrap();
        for (version, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 13) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, unixepoch())",
                rusqlite::params![version, name],
            )
            .unwrap();
        }
        let group_sql = MIGRATIONS
            .iter()
            .find(|(_, n, _)| *n == "group_judgement")
            .map(|(_, _, s)| *s)
            .unwrap();
        conn.execute_batch(group_sql).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (14, 'group_judgement', unixepoch())",
            [],
        )
        .unwrap();

        // 升级到当前 main：不许 duplicate column，且 ocr_fts 必须补上
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 再跑一次仍幂等
        let names: Vec<String> = conn
            .prepare("SELECT name FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (_, name, _) in MIGRATIONS {
            assert!(names.iter().any(|n| n == name), "缺迁移 {name}: {names:?}");
        }
        assert_eq!(names.len(), MIGRATIONS.len(), "账本应一迁一行: {names:?}");
        // 重编号那条顺延到账本尾部，不抢占 14
        assert!(cols_contains(&conn, "rule_verdict", "judgementScope"));
        let ocr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ocr_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ocr, 1, "旧库升级后必须补上 ocr_fts 虚表");
    }

    /// COMPLIANCE-TRISTATE（task）：0015 形态的旧库 → 0016 三态收敛的**逐类映射**
    /// 与行/id/证据保留。用旧 schema 手工建表 + 插每类一行，再执行 0016 的 SQL 本体
    /// （不靠 migrate() 全量，避免断言被前序迁移掩盖）。
    #[test]
    fn tristate_migration_maps_legacy_results_and_keeps_ids() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE assets (assetId INTEGER PRIMARY KEY, absPath TEXT NOT NULL);
             INSERT INTO assets (assetId, absPath) VALUES (1, '/x/a.jpg');
             -- 0011 + 0015 形态的 rule_verdict（旧 CHECK 只认 pass/fail/skipped/pending_human）
             CREATE TABLE rule_verdict (
                id            INTEGER PRIMARY KEY,
                packId        TEXT    NOT NULL,
                assetId       INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
                jobId         INTEGER,
                ruleId        TEXT    NOT NULL,
                result        TEXT    NOT NULL CHECK (result IN ('pass','fail','skipped','pending_human')),
                score         REAL,
                evidence      TEXT,
                fixAction     TEXT,
                fixTier       TEXT,
                recheckResult TEXT,
                ts            INTEGER NOT NULL,
                judgementScope TEXT,
                groupKey      TEXT
             );
             CREATE INDEX idx_rule_verdict_pack_rule ON rule_verdict(packId, ruleId);
             CREATE INDEX idx_rule_verdict_asset ON rule_verdict(assetId);
             CREATE INDEX idx_rule_verdict_job ON rule_verdict(jobId);
             CREATE INDEX idx_rule_verdict_group ON rule_verdict(packId, ruleId, groupKey);
             -- 0011 形态的 rule_feedback（0016 同样要收敛它的 machineResult 词表）
             CREATE TABLE rule_feedback (
                id            INTEGER PRIMARY KEY,
                packId        TEXT    NOT NULL,
                assetId       INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
                ruleId        TEXT    NOT NULL,
                machineResult TEXT    NOT NULL CHECK (machineResult IN ('pass','fail','pending_human')),
                humanResult   TEXT    NOT NULL CHECK (humanResult IN ('pass','fail')),
                reason        TEXT,
                ts            INTEGER NOT NULL
             );
             CREATE INDEX idx_rule_feedback_pack_rule ON rule_feedback(packId, ruleId);
             CREATE INDEX idx_rule_feedback_asset ON rule_feedback(assetId);
             INSERT INTO rule_feedback (packId, assetId, ruleId, machineResult, humanResult, reason, ts)
                 VALUES ('mini', 1, 'R_nofixer', 'pending_human', 'fail', '旧反馈', 0);",
        )
        .unwrap();
        let legacy = [
            (1, "R_pass", "pass", None, None),
            (2, "R_fail", "fail", None, None),
            (3, "R_skip", "skipped", None, None),
            (
                4,
                "R_nofixer",
                "pending_human",
                Some(r#"{"reason":"no_fixer"}"#),
                None,
            ),
            (
                5,
                "R_vlerr",
                "pending_human",
                Some(r#"{"reason":"rule_error","error":"vl timeout"}"#),
                None,
            ),
            (
                6,
                "R_grouperr",
                "pending_human",
                Some(r#"{"reason":"group_key_missing"}"#),
                None,
            ),
            (7, "R_fixed", "pass", None, Some("smart_crop")),
        ];
        for (id, rule, result, evidence, fix) in legacy {
            conn.execute(
                "INSERT INTO rule_verdict
                   (id, packId, assetId, jobId, ruleId, result, score, evidence, fixAction, ts,
                    judgementScope, groupKey)
                 VALUES (?1, 'mini', 1, 7, ?2, ?3, 0.5, ?4, ?5, 0, 'per_asset', NULL)",
                rusqlite::params![id, rule, result, evidence, fix],
            )
            .unwrap();
        }

        conn.execute_batch(include_str!("migrations/0016_tristate_verdict.sql"))
            .unwrap();

        // 逐类映射：no_fixer → fail；执行类 pending_human → undetermined；pass/fail/skipped 原样
        let got = |rule: &str| -> (String, Option<f64>, Option<String>) {
            conn.query_row(
                "SELECT result, confidence, fixAction FROM rule_verdict WHERE ruleId = ?1",
                [rule],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(got("R_pass").0, "pass");
        assert_eq!(got("R_fail").0, "fail");
        assert_eq!(got("R_skip").0, "skipped");
        assert_eq!(
            got("R_nofixer").0,
            "fail",
            "无修复器可修 = 不合格（旧 pending_human 映射）"
        );
        assert_eq!(got("R_vlerr").0, "undetermined", "执行错误 = 未判定");
        assert_eq!(got("R_grouperr").0, "undetermined");
        assert_eq!(got("R_fixed").0, "pass");
        assert_eq!(
            got("R_fixed").2.as_deref(),
            Some("smart_crop"),
            "修复证据必须保留"
        );
        // 行数/id 全保留；历史置信度不可知 → NULL（不造数）
        let (n, max_id, null_conf): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(id), SUM(confidence IS NULL) FROM rule_verdict",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((n, max_id, null_conf), (7, 7, 7));
        // 旧词已不可写入（新 CHECK 生效）
        assert!(conn
            .execute(
                "INSERT INTO rule_verdict (packId, assetId, ruleId, result, confidence, ts)
                 VALUES ('mini', 1, 'R_x', 'pending_human', 0.5, 0)",
                [],
            )
            .is_err());
        // rule_feedback 的 machineResult 同口径收敛（旧 pending_human → undetermined）
        let fb: String = conn
            .query_row(
                "SELECT machineResult FROM rule_feedback WHERE ruleId='R_nofixer'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fb, "undetermined");
        // judgementScope 兜底为 per_asset（组级传导依赖它进分组）
        let scope: String = conn
            .query_row(
                "SELECT judgementScope FROM rule_verdict WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scope, "per_asset");
    }
    /// COMPLIANCE-HUMAN（task）：0016 形态的**已安装库**升级到 0017 —— 只加表，
    /// 历史判定行零改动；活记录部分唯一索引拦住「双活」；空白 reason 被 schema CHECK 拒收。
    #[test]
    fn human_review_migration_adds_table_without_touching_verdicts() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_ledger(&conn).unwrap();
        // 手工重放 1..=16（模拟真机已装到 0016 的库；不走 migrate() 以免 0017 提前落位）
        for (v, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 16) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                rusqlite::params![v, name],
            )
            .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 16);
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/x/a.jpg', 'image', 'a.jpg', 'jpg', 10, 0, 1);
             INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, confidence, ts, judgementScope)
             VALUES ('mini', 1, 1, 'R1', 'fail', 0.42, 0, 'per_asset');",
        )
        .unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM rule_verdict", [], |r| r.get(0))
            .unwrap();

        migrate(&conn).unwrap();

        // 0017 落位后再把后面的迁移也跑完（0019 dup_edges 是 KI-2b 的边表）：
        // 断言「追平到最新版本」，而不是写死某一版
        assert_eq!(
            current_version(&conn).unwrap(),
            MIGRATIONS.iter().map(|(v, _, _)| *v).max().unwrap()
        );
        assert!(current_version(&conn).unwrap() >= 17);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM rule_verdict", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            before,
            "历史判定行一条都不能丢/改"
        );
        // N5b（0028）：结论值搬到 verdict 列，语义逐字不变（result 只表示「跑成了没有」）
        let (result, verdict, confidence): (String, Option<String>, Option<f64>) = conn
            .query_row(
                "SELECT result, verdict, confidence FROM rule_verdict WHERE assetId = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (result.as_str(), verdict.as_deref(), confidence),
            ("ok", Some("fail"), Some(0.42))
        );
        // 表在、索引在
        for idx in [
            "uq_verdict_human_review_active",
            "idx_verdict_human_review_asset",
            "idx_verdict_human_review_ts",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "缺索引 {idx}");
        }
        let insert = |rule: Option<&str>, status: &str, reason: &str, revoked: bool| {
            conn.execute(
                "INSERT INTO verdict_human_review
                   (packId, assetId, ruleId, judgementScope, groupKey, humanStatus, humanReason,
                    humanAt, revokedAt, revokeReason)
                 VALUES ('mini', 1, ?1, 'per_asset', '', ?2, ?3, 0, ?4, ?5)",
                rusqlite::params![
                    rule,
                    status,
                    reason,
                    revoked.then_some(0i64),
                    revoked.then_some("superseded")
                ],
            )
        };
        // 空白 reason 被 CHECK 拒收
        let e = insert(Some("R1"), "pass", "   ", false).unwrap_err();
        assert!(e.to_string().contains("CHECK"), "空白理由必须拒收: {e}");
        // 活记录唯一：第二条同键活记录被部分唯一索引拒收
        insert(Some("R1"), "pass", "改判理由", false).unwrap();
        let e = insert(Some("R1"), "fail", "另一条", false).unwrap_err();
        assert!(e.to_string().contains("UNIQUE"), "双活必须被拒: {e}");
        // 旧行落戳后可以再插（覆盖语义 = 先 revokedAt 落戳再插新行）；资产级（ruleId NULL）
        // 与规则级互不冲突
        conn.execute(
            "UPDATE verdict_human_review SET revokedAt = 0, revokeReason = 'superseded'
              WHERE packId = 'mini' AND assetId = 1 AND ruleId = 'R1' AND revokedAt IS NULL",
            [],
        )
        .unwrap();
        insert(Some("R1"), "fail", "覆盖后的结论", false).unwrap();
        insert(None, "pass", "整图改判", false).unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM verdict_human_review WHERE revokedAt IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 2, "规则级 1 条 + 资产级 1 条生效");
    }

    /// CMP-JOB-01（task）：0017 形态的**已安装库**升级到 0018 —— 只加表加列，
    /// 历史 job_queue 行零改动，且老行 `runId` 保持 NULL（归入「迁移前」伪 run，不回填）。
    #[test]
    fn compliance_runs_migration_adds_run_dimension_without_touching_history() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_ledger(&conn).unwrap();
        for (v, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 17) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                rusqlite::params![v, name],
            )
            .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 17);
        // 真机现场：30 条历史坏图留下的永久 failed 任务（runId 列还不存在）
        conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError, createdAt, updatedAt)
             VALUES ('compliance', 47, '{}', 'failed', 3, 'BATCH-01 坏字节 jpg', 0, 0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());

        let (kind, status, run_id): (String, String, Option<i64>) = conn
            .query_row(
                "SELECT kind, status, runId FROM job_queue WHERE targetId = 47",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), status.as_str()),
            ("compliance", "failed"),
            "历史行不许被改"
        );
        assert_eq!(run_id, None, "老行 runId 必须 NULL（不回填）");
        // 新表空着（历史不属于任何一次执行）
        let runs: i64 = conn
            .query_row("SELECT COUNT(*) FROM compliance_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 0);
        // 再跑一遍 migrate 不报错（升级二次调用幂等）
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
    }

    /// KI-2c（task）：pHashes（D4 八变体）迁移 —— 老库单值回填为 identity 变体。
    /// 验收口径：**连跑两次迁移，第二次不报错、不改已有行**。
    #[test]
    fn phash_d4_migration_backfills_identity_variant_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_ledger(&conn).unwrap();
        // 模拟真机已装到 0017 的库（手工重放，避免 migrate() 让新迁移提前落位）
        for (v, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 17) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                rusqlite::params![v, name],
            )
            .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 17);
        assert!(
            !cols_contains(&conn, "resource", "pHashes"),
            "0017 形态不该有 pHashes 列"
        );

        // 三种老数据：有 pHash（16 hex 大写，验证归一化）/ 无 pHash / 空串 pHash
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/x/a.jpg', 'image', 'a.jpg', 'jpg', 10, 0, 1),
                    (2, '/x/b.jpg', 'image', 'b.jpg', 'jpg', 10, 0, 1),
                    (3, '/x/c.jpg', 'image', 'c.jpg', 'jpg', 10, 0, 1);
             INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (1, 'primary', 0, 10, 10, 'ABCDEF0123456789'),
                    (2, 'primary', 0, 10, 10, NULL),
                    (3, 'primary', 0, 10, 10, '');",
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        let snap = |conn: &Connection| -> Vec<(i64, Option<String>, Option<String>)> {
            let mut stmt = conn
                .prepare("SELECT assetId, pHash, pHashes FROM resource ORDER BY assetId")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        let first = snap(&conn);
        assert_eq!(
            first[0].2.as_deref(),
            Some(r#"["abcdef0123456789"]"#),
            "老单值 → identity 变体（小写归一化）"
        );
        assert_eq!(
            first[0].1.as_deref(),
            Some("ABCDEF0123456789"),
            "老列原样不动"
        );
        assert_eq!(first[1].2, None, "无 pHash 的行不编造变体");
        assert_eq!(first[2].2, None, "空串 pHash 不编造变体");

        // 第二次迁移：不报错、账本不新增、行内容逐字节不变
        let ledger_before = applied_versions(&conn).unwrap().len();
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        assert_eq!(applied_versions(&conn).unwrap().len(), ledger_before);
        assert_eq!(snap(&conn), first, "第二次迁移不得改动任何已有行");

        // 已经写了多值变体的新行，重跑迁移也不能被单值回填覆盖（回填限定 pHashes IS NULL）
        conn.execute(
            "UPDATE resource SET pHashes = '[\"0000000000000001\",\"0000000000000002\"]'
              WHERE assetId = 1",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        let after: Option<String> = conn
            .query_row("SELECT pHashes FROM resource WHERE assetId = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after.as_deref(),
            Some(r#"["0000000000000001","0000000000000002"]"#),
            "已有八变体的行不许被单值回填覆盖"
        );
    }

    // ---------- N5b（0028）：结果行契约落库 ----------

    /// 造一张 0027 形态的库：几个资产 + 覆盖四种旧判定词的历史行。
    /// 返回迁移前的 (assetId, ruleId, result) 快照（顺序稳定，便于逐行比对）。
    fn legacy_v27_db() -> (Connection, Vec<(i64, String, String)>) {
        let conn = Connection::open_in_memory().unwrap();
        migrate_upto(&conn, 27).unwrap();
        for id in 1..=4 {
            conn.execute(
                "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, ?3, 'jpg', 10, 0, 0)",
                rusqlite::params![id, format!("/tmp/{id}.jpg"), format!("{id}.jpg")],
            )
            .unwrap();
        }
        for (asset, rule, result) in [
            (1_i64, "R1_size", "pass"),
            (2, "R1_size", "fail"),
            (3, "R1_size", "undetermined"),
            (4, "R1_size", "skipped"),
        ] {
            conn.execute(
                "INSERT INTO rule_verdict
                     (packId, assetId, jobId, ruleId, result, confidence, evidence, ts)
                 VALUES ('com.soujpg.pack.test', ?1, 7, ?2, ?3, 0.8, '{}', 0)",
                rusqlite::params![asset, rule, result],
            )
            .unwrap();
        }
        let snap = verdict_snapshot(&conn);
        (conn, snap)
    }

    /// 迁移后的行快照 (assetId, ruleId, result, verdict, verdict IS NULL)，按 assetId 排序。
    fn verdict_snapshot(conn: &Connection) -> Vec<(i64, String, String)> {
        let mut stmt = conn
            .prepare("SELECT assetId, ruleId, result FROM rule_verdict ORDER BY assetId, ruleId")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    fn contract_snapshot(conn: &Connection) -> Vec<(i64, String, String, Option<String>)> {
        let mut stmt = conn
            .prepare(
                "SELECT assetId, ruleId, result, verdict FROM rule_verdict
                 ORDER BY assetId, ruleId",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn verdict_split_keeps_every_conclusion_word_and_is_idempotent() {
        let (conn, before) = legacy_v27_db();
        assert_eq!(before.len(), 4);
        migrate(&conn).unwrap();

        let after = contract_snapshot(&conn);
        // 逐行核对：结论值**逐字不变**（pass/fail/undetermined），skipped 不冒充结论
        assert_eq!(
            after,
            vec![
                (1, "R1_size".to_string(), "ok".to_string(), Some("pass".to_string())),
                (2, "R1_size".to_string(), "ok".to_string(), Some("fail".to_string())),
                (
                    3,
                    "R1_size".to_string(),
                    "ok".to_string(),
                    Some("undetermined".to_string())
                ),
                (4, "R1_size".to_string(), "skipped".to_string(), None),
            ],
            "旧 result=pass/fail/undetermined → result=ok + verdict 原值；skipped → result=skipped + verdict NULL"
        );

        // 幂等：再跑一遍，行内容逐字节不变、账本不新增
        let ledger = applied_versions(&conn).unwrap();
        let digest = format!("{:?}", after);
        migrate(&conn).unwrap();
        assert_eq!(contract_snapshot(&conn), after, "第二次迁移不得改动任何行");
        assert_eq!(
            applied_versions(&conn).unwrap(),
            ledger,
            "第二次迁移不得新增账本行"
        );
        assert_eq!(
            format!("{:?}", contract_snapshot(&conn)),
            digest,
            "digest 两遍一致"
        );
        // 版本号不钉死 28：N1 的 0029（plugin_spec）在合流时排在 0028 之后，
        // 全链跑到顶 = top_version()。本用例要守的是「搬值语义 + 幂等」，不是某一段的序号。
        assert_eq!(current_version(&conn).unwrap(), top_version());
    }

    #[test]
    fn contract_checks_reject_unknown_words_and_allow_null_verdict() {
        let (conn, _) = legacy_v27_db();
        migrate(&conn).unwrap();
        // verdict 可空（处理类行：没有审核结论），result 只认三值
        conn.execute(
            "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, ts)
             VALUES ('p', 1, 1, 'R_batch', 'ok', NULL, 1.0, 0)",
            [],
        )
        .unwrap();
        // 自造状态一律拒收（前端不可渲染的根因就在这里拦）
        for bad in ["pending_human", "repaired", "optimized"] {
            let err = conn
                .execute(
                    "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, ts)
                     VALUES ('p', 1, 1, 'R_bad', 'ok', ?1, 1.0, 0)",
                    [bad],
                )
                .unwrap_err();
            assert!(err.to_string().contains("CHECK"), "{bad} 必须被 CHECK 拒收");
        }
    }

    #[test]
    fn packs_kinds_defaults_to_judge_and_rejects_unknown_words() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO packs (id, version, dir, manifest_hash, api_version, installed_at)
             VALUES ('p.legacy', '1.0.0', '/p', 'h', '1.0', 0)",
            [],
        )
        .unwrap();
        let kinds: String = conn
            .query_row("SELECT kinds FROM packs WHERE id = 'p.legacy'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            kinds, "judge",
            "旧包（未声明 kinds）= 判定类，与 effective_kinds 回落同口径"
        );

        conn.execute(
            "INSERT INTO packs (id, version, dir, manifest_hash, api_version, kinds, installed_at)
             VALUES ('p.both', '1.0.0', '/p2', 'h2', '1.0', 'judge,process', 0)",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO packs (id, version, dir, manifest_hash, api_version, kinds, installed_at)
                 VALUES ('p.bad', '1.0.0', '/p3', 'h3', '1.0', 'analyzer', 0)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("CHECK"),
            "kinds 闭集外的值必须拒收"
        );
    }

    /// DM-A4（task）：0033 给 `assets` 加 `batchId`（资产级批次溯源）。
    ///
    /// 验收口径：
    /// 1. 真机已装到 0032 的老库升级后列在位、索引在位，`top_version()` 跟上；
    /// 2. **历史行保持 NULL**（不按 rootPath 回填 —— 老库里「唯一命中」本来就不唯一，
    ///    回填等于编造来源；老库的 batch 范围由 dedup_api 的 approx 分支明示近似）；
    /// 3. 连跑两次 `migrate` 不报错（账本幂等，不会重复 ALTER）。
    #[test]
    fn asset_batch_id_migration_adds_nullable_column_without_backfill() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_ledger(&conn).unwrap();
        for (v, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v <= 32) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                rusqlite::params![v, name],
            )
            .unwrap();
        }
        assert_eq!(current_version(&conn).unwrap(), 32);
        // 老库现场：一个批次 + 一张批次根目录下的资产（还没有 batchId 列）
        conn.execute(
            "INSERT INTO import_batches (batchId, rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
             VALUES (7, '/lib/dirA', 'reference', 'done', 1, 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/lib/dirA/a.jpg', 'a.jpg', 'jpg', 10, 1, 0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        assert!(
            cols_contains(&conn, "assets", "batchId"),
            "0033 必须加 assets.batchId"
        );
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_assets_batch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "批次列必须带索引（range=batch 等值查）");

        // 历史行 NULL：不假装归属
        let old: Option<i64> = conn
            .query_row("SELECT batchId FROM assets WHERE assetId = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(old, None, "迁移不许回填历史行的批次归属");

        // 新行能写、能按批次等值查（消费端口径）
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, batchId)
             VALUES (2, '/lib/dirA/b.jpg', 'b.jpg', 'jpg', 10, 1, 0, 7)",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets WHERE batchId = 7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1, "按批次等值只应命中归属该批的那一行");

        // 升级二次调用幂等
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
    }

    /// DM-C1（task）：`0034_ocr_boxes` 幂等 + 头版本对齐 + 迁移后表可读写。
    /// 合流后 0033 = asset_batch_id（DM-A4）、0034 = ocr_boxes（本卡），两者不冲突。
    #[test]
    fn ocr_boxes_migration_idempotent_and_usable() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        assert!(top_version() >= 34, "本卡迁移号 0034");

        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/tmp/a.jpg', 'image', 'a.jpg', 'jpg', 1, 0, 0)",
            [],
        )
        .unwrap();
        let one = crate::ocr_boxes::OcrBox {
            x0: 0.1,
            y0: 0.2,
            x1: 0.5,
            y1: 0.4,
            text: "水印".into(),
            box_score: 0.9,
            text_score: 0.8,
        };
        assert_eq!(
            crate::ocr_boxes::replace(&conn, 1, std::slice::from_ref(&one)).unwrap(),
            1
        );
        assert_eq!(crate::ocr_boxes::count(&conn, 1).unwrap(), 1);

        // 二次迁移幂等：不重放、不丢数据
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        assert_eq!(crate::ocr_boxes::load(&conn, 1).unwrap(), vec![one]);
    }

    /// DECouple-BE（task）：0035 把 0017 的人审行**幂等**搬进 `human_review`。
    ///
    /// 这是「存量迁移」的 Rust 侧证据（生产脚本 `scripts/migrate_human_review_v2.py` 负责
    /// 备份/回读校验/purge）。断言三件事：
    /// 1. 搬到新表后**键换了**：`schemeId = 'pack:' || 旧 packId`，`legacyId` 指向旧行 id；
    /// 2. **内容版本快照**落库：取 `asset_versions(current=1).sha256 ?? assets.fileHash ?? ''`；
    /// 3. 重复 `migrate()` 不会长出第二行（幂等）。
    #[test]
    fn human_review_v2_migrates_legacy_rows_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        // 造「0034 形态」的旧库
        migrate_upto(&conn, 34).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 34);
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, fileHash, importedAt)
             VALUES (1, '/x/a.jpg', 'image', 'a.jpg', 'jpg', 10, 0, 'hash-a', 1),
                    (2, '/x/b.jpg', 'image', 'b.jpg', 'jpg', 10, 0, NULL, 1);",
        )
        .unwrap();
        // 资产 2 有当前工作版本（修图产物）→ 内容版本取工作版本 sha256，而不是 fileHash
        conn.execute_batch(
            "INSERT INTO asset_versions (assetId, packId, actions, result, chain, createdAt, sha256, current, packVersion, pluginHash)
             VALUES (2, 'com.soujpg.pack.ecommerce', '[]', 'ok', '[]', 1, 'work-sha', 1, '1.0.0', 'h');",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO verdict_human_review (id, packId, assetId, ruleId, judgementScope, groupKey, humanStatus, humanReason, humanAt, machineResult)
             VALUES (7, 'com.soujpg.pack.ecommerce', 1, 'R1_size', 'per_asset', '', 'pass', '阈值过严', 100, 'fail'),
                    (8, 'com.soujpg.pack.ecommerce', 2, NULL, 'per_asset', '', 'fail', '整图模糊', 101, 'undetermined');",
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), top_version());
        assert!(top_version() >= 35, "本卡迁移号 0035");

        let rows: Vec<(i64, String, Option<String>, String, String)> = conn
            .prepare(
                "SELECT legacyId, schemeId, ruleId, humanStatus, contentVersion
                   FROM human_review ORDER BY legacyId",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2, "两条旧人审都要搬过来");
        assert_eq!(rows[0].0, 7);
        assert_eq!(
            rows[0].1, "pack:com.soujpg.pack.ecommerce",
            "schemeId 带 pack: 前缀"
        );
        assert_eq!(rows[0].2.as_deref(), Some("R1_size"));
        assert_eq!(rows[0].3, "pass");
        assert_eq!(rows[0].4, "hash-a", "无工作版本 → 取导入 fileHash");
        assert_eq!(rows[1].0, 8);
        assert!(rows[1].2.is_none(), "资产级人审 ruleId 仍为 NULL");
        assert_eq!(rows[1].4, "work-sha", "有工作版本 → 取工作版本 sha256");

        // 幂等：再跑一遍不长新行
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM human_review", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "重复迁移不得复制人审行");
    }
}
