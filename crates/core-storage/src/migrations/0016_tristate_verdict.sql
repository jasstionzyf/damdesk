-- 0016 tristate_verdict — COMPLIANCE-TRISTATE（卡 t_d40e2d33）。
--
-- 裁决（2026-10-17 用户）：判定结果**只有三态** pass / fail / undetermined，不再有
-- pending_human（待人工）/ repaired（修复后合格）/ failed 这类派生态；每条规则结果**必带
-- confidence ∈ [0,1]**（检测器/插件给出，原始 score 只作证据不参与比较）。
--
-- 为什么必须重建表：0011 的 `result` 带 CHECK (pass|fail|skipped|pending_human)，
-- SQLite 不能改 CHECK —— 只能新建表 → 按映射搬行 → 删旧表 → 改名（迁移纪律「只加不改不删」
-- 的例外只此一处，且**行数/主键 id 全部保留**，历史判定不丢）。
--
-- 旧值映射（每类计数见 reports/COMPLIANCE-TRISTATE.md；脚本 scripts/tristate_migrate_counts.py）：
--   pending_human + evidence.reason = 'no_fixer'  → fail
--     （语义：包里没有修复器可修 = 该规则确实不合格，不是「机器不敢判」；
--      房产包 fixer:null 还配 retry_then_human，正是「27 张里不合格 0」的根因）
--   pending_human + 执行类原因（load_error / rule_error / model_missing / vl_timeout /
--     vl_invalid_answer / group_key_missing / group_incomplete / fixer_not_available /
--     external_data …）                          → undetermined
--   failed → undetermined（旧实现的「失败」= 判定过程出错，不是不合格结论）
--   repaired → pass（修复后复检通过；fixAction/fixTier/recheckResult 三列原样保留作证据）
--   pass / fail / skipped                       → 原样
--
-- confidence：历史行的置信度**不可知** → 留 NULL（不造数）。写入路径（NewVerdict）用
-- 非 Option 的 f64 强约束，新行不可能写出 NULL；汇总时 NULL 行不参与「资产置信度 = 最小值」。
-- 这也是 `confidence` 允许 NULL 的唯一理由（迁移兼容），不是「可以不给」。

CREATE TABLE rule_verdict_v16 (
    id            INTEGER PRIMARY KEY,
    packId        TEXT    NOT NULL,
    assetId       INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    jobId         INTEGER,
    ruleId        TEXT    NOT NULL,
    -- 三态 + skipped（depends_on 短路 / 打标型 / 作用域不符 → 该规则对这张图没有结论）
    result        TEXT    NOT NULL CHECK (result IN ('pass','fail','undetermined','skipped')),
    -- 0..1；NULL 只可能出现在 0016 之前的历史行（见文件头）
    confidence    REAL    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    score         REAL,
    evidence      TEXT,
    fixAction     TEXT,
    fixTier       TEXT    CHECK (fixTier IS NULL OR fixTier IN ('guaranteed','best_effort')),
    recheckResult TEXT    CHECK (recheckResult IS NULL OR recheckResult IN ('pass','fail')),
    ts            INTEGER NOT NULL,
    judgementScope TEXT   CHECK (judgementScope IS NULL OR judgementScope IN ('per_asset','per_group')),
    groupKey      TEXT
);

INSERT INTO rule_verdict_v16
    (id, packId, assetId, jobId, ruleId, result, confidence, score, evidence,
     fixAction, fixTier, recheckResult, ts, judgementScope, groupKey)
SELECT
    id, packId, assetId, jobId, ruleId,
    CASE
        WHEN result = 'pending_human'
             AND COALESCE(json_extract(evidence, '$.reason'), '') = 'no_fixer' THEN 'fail'
        WHEN result = 'pending_human' THEN 'undetermined'
        WHEN result = 'failed'        THEN 'undetermined'
        WHEN result = 'repaired'      THEN 'pass'
        ELSE result
    END,
    NULL                       -- confidence：历史行不可知，不造数
    , score, evidence, fixAction, fixTier, recheckResult, ts,
    COALESCE(judgementScope, 'per_asset'), groupKey
FROM rule_verdict;

DROP TABLE rule_verdict;
ALTER TABLE rule_verdict_v16 RENAME TO rule_verdict;

CREATE INDEX idx_rule_verdict_pack_rule ON rule_verdict(packId, ruleId);
CREATE INDEX idx_rule_verdict_asset ON rule_verdict(assetId);
CREATE INDEX idx_rule_verdict_job ON rule_verdict(jobId);
CREATE INDEX idx_rule_verdict_group ON rule_verdict(packId, ruleId, groupKey);
-- 低置信度复核视图（UI3 子卡的入口；阈值在查询里给，索引只固化「按 pack 找低分」）
CREATE INDEX idx_rule_verdict_confidence ON rule_verdict(packId, confidence);

-- rule_feedback.machineResult 同口径收敛：旧 CHECK 只认 pass/fail/pending_human，
-- 三态口径下人工复核的对象可能是 undetermined（机器没结论），必须能落库。
-- 旧行里 pending_human → undetermined（与 rule_verdict 同映射），id/理由保留。
CREATE TABLE rule_feedback_v16 (
    id            INTEGER PRIMARY KEY,
    packId        TEXT    NOT NULL,
    assetId       INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    ruleId        TEXT    NOT NULL,
    machineResult TEXT    NOT NULL CHECK (machineResult IN ('pass','fail','undetermined')),
    humanResult   TEXT    NOT NULL CHECK (humanResult IN ('pass','fail')),
    reason        TEXT,
    ts            INTEGER NOT NULL
);
INSERT INTO rule_feedback_v16 (id, packId, assetId, ruleId, machineResult, humanResult, reason, ts)
SELECT id, packId, assetId, ruleId,
       CASE WHEN machineResult = 'pending_human' THEN 'undetermined' ELSE machineResult END,
       humanResult, reason, ts
FROM rule_feedback;
DROP TABLE rule_feedback;
ALTER TABLE rule_feedback_v16 RENAME TO rule_feedback;
CREATE INDEX idx_rule_feedback_pack_rule ON rule_feedback(packId, ruleId);
CREATE INDEX idx_rule_feedback_asset ON rule_feedback(assetId);
