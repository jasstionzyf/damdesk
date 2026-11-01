-- 0028 result_verdict_contract — N5b（卡 t_4c0b3767）。
--
-- 落 N5a 定下的**结果行契约**（权威：damdesk-product/references/plugin-result-contract.md，
-- 协议 §4.4）：执行结果 ≠ 判定结论。
--
--   result  = 这一行跑成了没有（所有插件都有）：ok | failed | skipped
--   verdict = 审核结论（**只有判定类**才有）：pass | fail | undetermined，处理类行没有 →
--             可空（这就是「verdict 行改可空」：列可空，不是表可空）
--
-- 迁移前 `rule_verdict.result` 一列混着两种语义（pass/fail/undetermined 是结论，skipped 是
-- 「没跑出结论」）。这里按契约拆成两列，**旧行的结论值逐字保留**（不是改写成别的词）：
--
--   旧 result = pass/fail/undetermined → result='ok' + verdict=<原值>
--   旧 result = skipped               → result='skipped' + verdict=NULL
--
-- 为什么必须重建表：SQLite 不能改 CHECK 约束（`result` 要换成三值闭集、`verdict` 要可空），
-- 与 0016 同法重建（id 原样搬，判定历史一条不丢）。
--
-- 同时：插件安装记录落 `kinds`（0026 packs 表）——宿主据此决定有没有审核列/审核页，
-- **不靠猜**（不再从 manifest 现场读，覆盖「包的目录被删/被换掉」的情况）。
-- 规范化取值 = 逗号分隔、按 judge,process 固定顺序（闭集，省掉 JSON 解析与顺序漂移）。

CREATE TABLE rule_verdict_v28 (
    id             INTEGER PRIMARY KEY,
    packId         TEXT    NOT NULL,
    assetId        INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    jobId          INTEGER,
    ruleId         TEXT    NOT NULL,
    -- 执行结果三值（所有插件/所有行都有）
    result         TEXT    NOT NULL CHECK (result IN ('ok','failed','skipped')),
    -- 审核结论三态（只有判定类行有；处理类行 = NULL。NULL 不是「跑失败」，
    -- 跑失败看 result='failed'）
    verdict        TEXT    CHECK (verdict IS NULL OR verdict IN ('pass','fail','undetermined')),
    -- 0..1；NULL 只可能出现在 0016 之前的历史行（见 0016 文件头）
    confidence     REAL    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    score          REAL,
    evidence       TEXT,
    fixAction      TEXT,
    fixTier        TEXT    CHECK (fixTier IS NULL OR fixTier IN ('guaranteed','best_effort')),
    recheckResult  TEXT    CHECK (recheckResult IS NULL OR recheckResult IN ('pass','fail')),
    ts             INTEGER NOT NULL,
    judgementScope TEXT    CHECK (judgementScope IS NULL OR judgementScope IN ('per_asset','per_group')),
    groupKey       TEXT,
    -- IP-K05（0027）：产出该行的包版本 / manifest.json 原文 sha256
    packVersion    TEXT,
    pluginHash     TEXT
);

INSERT INTO rule_verdict_v28
    (id, packId, assetId, jobId, ruleId, result, verdict, confidence, score, evidence,
     fixAction, fixTier, recheckResult, ts, judgementScope, groupKey, packVersion, pluginHash)
SELECT
    id, packId, assetId, jobId, ruleId,
    -- 旧 skipped = 这条规则对这张图没有结论 → 契约里的「执行跳过」
    CASE WHEN result = 'skipped' THEN 'skipped' ELSE 'ok' END,
    -- 旧 pass/fail/undetermined 逐字搬进 verdict（结论值不变）
    CASE WHEN result = 'skipped' THEN NULL ELSE result END,
    confidence, score, evidence, fixAction, fixTier, recheckResult, ts,
    judgementScope, groupKey, packVersion, pluginHash
FROM rule_verdict;

DROP TABLE rule_verdict;
ALTER TABLE rule_verdict_v28 RENAME TO rule_verdict;

-- 重建 0011/0015/0016 的索引（重建表会连带删掉它们）
CREATE INDEX idx_rule_verdict_pack_rule ON rule_verdict(packId, ruleId);
CREATE INDEX idx_rule_verdict_asset ON rule_verdict(assetId);
CREATE INDEX idx_rule_verdict_job ON rule_verdict(jobId);
CREATE INDEX idx_rule_verdict_group ON rule_verdict(packId, ruleId, groupKey);
-- 低置信度复核视图（UI3 子卡的入口；阈值在查询里给，索引只固化「按 pack 找低分」）
CREATE INDEX idx_rule_verdict_confidence ON rule_verdict(packId, confidence);
-- 0027：定位「某份包代码影响过哪些判定」（争议回溯/重判范围计算）
CREATE INDEX idx_rule_verdict_plugin_hash
    ON rule_verdict(pluginHash) WHERE pluginHash IS NOT NULL;

-- 插件安装记录的种类（judge/process，可同时具备）。
-- 缺省 'judge' = 与 manifest 未声明 kinds 的旧包同口径（N5a `effective_kinds` 的回落），
-- 历史行不需要造数：0026 之前登记的包都是判定包。
ALTER TABLE packs ADD COLUMN kinds TEXT NOT NULL DEFAULT 'judge'
    CHECK (kinds IN ('judge','process','judge,process'));
