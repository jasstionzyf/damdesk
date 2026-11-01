-- 0017 human_review — COMPLIANCE-HUMAN（卡 t_03127b19）。
--
-- 人工二次审核：机器结论（`rule_verdict`）**永不改写**（可审计、可复现），人工改判单独落一张表，
-- 生效结论 = `humanStatus ?? result`（唯一口径见 `svc_library::compliance_view::ReviewIndex`）。
--
-- 为什么独立表而不是给 rule_verdict 加列：
-- 1. rule_verdict 是「判定过程」的事实表，每个 job 一批行；人工结论属于「业务裁决」，
--    生命周期与批次无关（重判后仍生效），混在一张表里口径必然打架；
-- 2. 一行判定可被多次改判 → 需要保留历史（撤销/覆盖都要可追），
--    而 rule_verdict 每 (asset,rule,scope) 只有最新一行语义。
--
-- 语义要点：
-- * `ruleId IS NULL` = **资产级整体结论**（改判入口 ruleId 缺省时的落点）：该资产最终状态直接
--   等于 humanStatus，不再看逐规则汇总；
-- * `judgementScope` / `groupKey` 只作**审计记录**（用户从哪条结果改的），
--   生效匹配只按 `(packId, assetId, ruleId)` —— 否则「组级行改成逐图行」会让改判静默失效；
-- * `machineResult` / `machineConfidence` = 改判当时机器结论的**快照**（审计对照用）；
--   生效判定永远按当前 `rule_verdict` 计算，快照不参与口径（重判后不假装是最新机器结论）；
-- * 只追加不原地改写：重复改判 = 旧记录 `revokedAt` 落戳（revokeReason='superseded'）+ 插入新记录；
--   撤销 = 活记录 `revokedAt` 落戳。**活记录（revokedAt IS NULL）唯一**：部分唯一索引兜底
--   「同一 (pack, asset, rule|资产级) 只能有一条生效改判」，并发/重复提交不会造出双活。

CREATE TABLE verdict_human_review (
    id                INTEGER PRIMARY KEY,
    packId            TEXT    NOT NULL,
    assetId           INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    -- NULL = 资产级整体结论
    ruleId            TEXT,
    judgementScope    TEXT    NOT NULL DEFAULT 'per_asset'
                      CHECK (judgementScope IN ('per_asset','per_group')),
    groupKey          TEXT    NOT NULL DEFAULT '',
    -- 人工最终结论（只有两档：合格 / 不合格；「未判定」是机器给不出结论，不是人的裁决）
    humanStatus       TEXT    NOT NULL CHECK (humanStatus IN ('pass','fail')),
    -- 改判理由：**必填**（CHECK 兜底空白/纯空格；接口层另有一道 trim 校验给中文错误码）
    humanReason       TEXT    NOT NULL CHECK (length(trim(humanReason)) > 0),
    humanAt           INTEGER NOT NULL,
    reviewedBy        TEXT,
    -- 改判当时的机器结论快照（可空：历史/未跑场景）
    machineResult     TEXT    CHECK (machineResult IS NULL OR machineResult IN ('pass','fail','undetermined')),
    machineConfidence REAL    CHECK (machineConfidence IS NULL
                                     OR (machineConfidence >= 0.0 AND machineConfidence <= 1.0)),
    -- 撤销/被新改判覆盖的时间戳（NULL = 生效中）
    revokedAt         INTEGER,
    revokeReason      TEXT
);

-- 活记录唯一：COALESCE(ruleId,'*') 让「资产级」也参与唯一性（SQLite 里多个 NULL 互不冲突）
CREATE UNIQUE INDEX uq_verdict_human_review_active
    ON verdict_human_review(packId, assetId, COALESCE(ruleId, '*'))
    WHERE revokedAt IS NULL;

CREATE INDEX idx_verdict_human_review_asset ON verdict_human_review(packId, assetId);
CREATE INDEX idx_verdict_human_review_ts ON verdict_human_review(packId, humanAt);
