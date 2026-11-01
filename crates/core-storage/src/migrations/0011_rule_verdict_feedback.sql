-- 0011 rule_verdict / rule_feedback — C0（合规判定引擎，PLAN.md §6.3）。
-- 迁移纪律「只加不改不删」：纯加两张新表。
--
-- 判定/反馈按 (packId, ruleId) 归属：同刻只激活一个 pack，切换即整体替换，
-- 历史判定保留可查（换包不丢）。计划文档中的 imageSN 在本地 = assets.assetId
-- （本地主键即 SN 等价物，全库沿用 assetId 命名）。
--
-- rule_verdict：每张图每规则一行判定（编排器逐步落）。
--   result: pass | fail | skipped（depends_on 短路/未执行） | pending_human（best_effort 复检仍败 /
--           external_data 不可自动判定，转人工队列）
--   score: 检测器分数（如 fg_ratio=0.72；无分数检测器为 NULL）
--   evidence: json 检测器原始输出/阈值/证据（判定可复现；已有标签不作为合格结论证据）
--   fixAction: 修复插件 id；fixTier: guaranteed | best_effort；recheckResult: 复检结果（NULL=未复检）
--
-- rule_feedback：review 界面「误判」按钮写入（C6）；v1 只落库 + CSV 导出。

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
    fixTier       TEXT    CHECK (fixTier IS NULL OR fixTier IN ('guaranteed','best_effort')),
    recheckResult TEXT    CHECK (recheckResult IS NULL OR recheckResult IN ('pass','fail')),
    ts            INTEGER NOT NULL
);
CREATE INDEX idx_rule_verdict_pack_rule ON rule_verdict(packId, ruleId);
CREATE INDEX idx_rule_verdict_asset ON rule_verdict(assetId);
CREATE INDEX idx_rule_verdict_job ON rule_verdict(jobId);

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
