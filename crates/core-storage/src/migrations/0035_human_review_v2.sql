-- 0035 human_review — DECouple-BE（卡 t_59942f03）：人审从「资产域附属」升为**独立覆盖层**。
--
-- 用户裁决（2026-10-31）：图库域与 job 域完全解耦。数据层模型 =
--   **固有归资产、过程归 job、人审是带失效条件的覆盖层**（不存在 verdict_current 窄表，
--   「当前生效结论」一律查询时由 job 账本 + 本表合成）。
--
-- 为什么是**新表**而不是给 0017 加列：
-- * 键变了：0017 是 `(packId, assetId, ruleId)`；本表是 `(assetId, schemeId, ruleId)` ——
--   schemeId 是**方案身份**（当前判定账本只记 packId，故落库值 = `pack:<packId>`；
--   规格化后可用 `spec:<id>` 前缀）。人审跨 job 生效（从不绑 run/job）；
-- * 少了「资产域」语义：0017 把判断挂在 pack 名下，资产视图读它就会把判定概念漏进图库域；
--   本表只被 job 域（L2/L3）读；
-- * **带失效条件**：`contentVersion` 记录改判时的资产内容版本，读侧只认与当前内容版本相等的
--   活记录 —— 资产重新导入 / 修图产生新工作版本后，旧人审自动失效（不删行，可审计），
--   这正是 0017 缺的那一列（实测：修图后旧人审仍然生效，会给出错误结论）。
--
-- 语义硬口径：
-- * `ruleId IS NULL` = **资产级整体结论**（整张图一条裁决）；
-- * 只追加不原地改写：重复改判 = 旧活记录 `revokedAt` 落戳（revokeReason='superseded'）
--   + 插新行；撤销 = 活记录落戳。活记录唯一由部分唯一索引兜底；
-- * `machineResult` / `machineConfidence` = 改判当时机器结论的快照（审计对照，不参与口径）；
-- * `humanStatus` 只有两档：pass / fail（「未判定」是机器给不出结论，不是人的裁决）。
--
-- contentVersion 口径（读侧同一表达式，见 core-storage `compliance_sqlite::CURRENT_CONTENT_VERSION_SQL`）：
--   `COALESCE(asset_versions(current=1).sha256, assets.fileHash, '')`
-- 两个来源都是内容 hash：修图产出的工作版本带 sha256（0032），原地索引资产带导入 sha256（0001）。

CREATE TABLE IF NOT EXISTS human_review (
    id                INTEGER PRIMARY KEY,
    assetId           INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    -- 判定方案身份（跨 job 生效；当前 = 'pack:<packId>'）
    schemeId          TEXT    NOT NULL,
    -- NULL = 资产级整体结论
    ruleId            TEXT,
    judgementScope    TEXT    NOT NULL DEFAULT 'per_asset'
                      CHECK (judgementScope IN ('per_asset','per_group')),
    groupKey          TEXT    NOT NULL DEFAULT '',
    humanStatus       TEXT    NOT NULL CHECK (humanStatus IN ('pass','fail')),
    humanReason       TEXT    NOT NULL CHECK (length(trim(humanReason)) > 0),
    judgedAt          INTEGER NOT NULL,
    reviewedBy        TEXT,
    -- 改判时的资产内容版本（'' = 资产既无工作版本也无导入 hash：只在测试/空库出现）
    contentVersion    TEXT    NOT NULL DEFAULT '',
    machineResult     TEXT    CHECK (machineResult IS NULL OR machineResult IN ('pass','fail','undetermined')),
    machineConfidence REAL    CHECK (machineConfidence IS NULL
                                     OR (machineConfidence >= 0.0 AND machineConfidence <= 1.0)),
    revokedAt         INTEGER,
    revokeReason      TEXT,
    -- 0017 行原 id（迁移可追溯；新行为 NULL）
    legacyId          INTEGER
);

-- 活记录唯一：同一 (asset, scheme, rule|资产级) 只能有一条生效人审
CREATE UNIQUE INDEX IF NOT EXISTS uq_human_review_active
    ON human_review(assetId, schemeId, COALESCE(ruleId, '*'))
    WHERE revokedAt IS NULL;

-- 按资产取人审（job 域下钻/L3 合成的唯一查询路径）
CREATE INDEX IF NOT EXISTS idx_human_review_asset ON human_review(assetId, schemeId);
-- 按方案取人审（合成整批结果时按 scheme 一次拉全量活记录）
CREATE INDEX IF NOT EXISTS idx_human_review_scheme ON human_review(schemeId, judgedAt);
-- 迁移幂等锚点：同一条 0017 行只搬一次
CREATE UNIQUE INDEX IF NOT EXISTS uq_human_review_legacy
    ON human_review(legacyId) WHERE legacyId IS NOT NULL;

-- ---------------------------------------------------------------- 存量迁移（幂等）
--
-- 0017 → 0035：packId → 'pack:'||packId；contentVersion 取当前内容版本。
-- INSERT OR IGNORE + legacyId 唯一索引 ⇒ 重复执行不会长出第二条（脚本/启动迁移都安全）。
INSERT OR IGNORE INTO human_review
    (assetId, schemeId, ruleId, judgementScope, groupKey, humanStatus, humanReason,
     judgedAt, reviewedBy, contentVersion, machineResult, machineConfidence,
     revokedAt, revokeReason, legacyId)
SELECT
    v.assetId,
    'pack:' || v.packId,
    v.ruleId,
    v.judgementScope,
    v.groupKey,
    v.humanStatus,
    v.humanReason,
    v.humanAt,
    v.reviewedBy,
    COALESCE(
        (SELECT av.sha256 FROM asset_versions av
          WHERE av.assetId = v.assetId AND av.current = 1),
        (SELECT a.fileHash FROM assets a WHERE a.assetId = v.assetId),
        ''
    ),
    v.machineResult,
    v.machineConfidence,
    v.revokedAt,
    v.revokeReason,
    v.id
FROM verdict_human_review v
-- 已被撤销的旧记录迁移后仍为撤销态（历史审计）；活记录唯一索引按 (asset, scheme, rule) 兜底
ORDER BY v.id;
