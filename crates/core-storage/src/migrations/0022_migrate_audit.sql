-- 0022 migrate_audit —— 「一键迁入库」的明细账本（卡 task / DT6）。
--
-- 一次迁移动作 = 一个 batchId；每个真正搬过的资产落一行：谁（operator）、从哪到哪
-- （fromPath → toPath）、搬了多少字节（bytes）、何时（at）、结果（status）。
-- 卡里写的是 0021 —— 已被 TAG-VALUE 占用，按迁移纪律顺延到 0022（只改编号）。
--
-- 为什么独立表而不是塞进 assets：迁移是**动作**不是资产属性。一次资产可能被迁移多次
-- （先失败后重试、库根换过），历史必须逐次可查；assets 上只有最新态（absPath/storeMode）。
--
-- status 两态：'done'（文件已落库 + assets 已切 copyin）/ 'failed'（本次失败，
-- 资产仍指向原路径、库内不留半成品）。失败行保留 error 供重试定位，历史行不改写。
--
-- operator：单机形态没有用户身份 → 默认 'local'（请求可覆盖）。留列是为了「人」这一项
-- 审计在将来多用户形态下不用改表。

CREATE TABLE migrate_audit (
    id        INTEGER PRIMARY KEY,
    batchId   TEXT    NOT NULL,
    kind      TEXT    NOT NULL CHECK (kind IN ('copyin','relocate')),
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    fromPath  TEXT    NOT NULL,
    toPath    TEXT    NOT NULL,
    bytes     INTEGER NOT NULL DEFAULT 0,
    operator  TEXT    NOT NULL DEFAULT 'local',
    status    TEXT    NOT NULL CHECK (status IN ('done','failed')),
    error     TEXT,
    at        INTEGER NOT NULL
);
CREATE INDEX idx_migrate_audit_batch ON migrate_audit(batchId, status);
CREATE INDEX idx_migrate_audit_asset ON migrate_audit(assetId);
