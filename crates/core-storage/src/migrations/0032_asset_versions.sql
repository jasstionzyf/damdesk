-- 0029 asset_versions — N6e（卡 t_b6c2aa1e）宿主侧动作链的产物账本。
--
-- 一句话：**一次动作链 = 一行结果 + 至多一张工作版本**。
--
-- 为什么单独一张表（不是塞进 rule_verdict）：
-- * 处理类插件（manifest.kinds 含 process）没有审核结论 —— `rule_verdict.verdict` 为空的
--   判定行会污染「未判定」统计，也会让「这张图判过没有」的候选查询误以为处理过；
-- * 产物是**文件事实**（path/sha256/尺寸/格式 + 逐步链证据），与判定结论生命周期不同：
--   同一资产可以反复处理（每次生成一版，旧的自动降级为历史版本）。
--
-- 口径（与 `plugin-result-contract.md` / `plugin-design-and-ui.md` §7 一致）：
-- * `result='ok'`：链跑通，`path/sha256` 是**链尾动作的产物**（中间产物用完即删，不入库）；
-- * `result='failed'`：链中途失败/取消/超时，只留**结果行**（`errorCode/errorMessage` 指向失败动作，
--   `chain` 保留已完成的部分），**不留半成品工作版本**（`path/sha256` 为 NULL、`current=0`）；
-- * `current=1` = 该资产**当前**工作版本，一个资产最多一张（DB 唯一索引兜底，不靠代码自觉）；
-- * `actions` = 本次链的动作清单（JSON 数组，顺序即执行序），`chain` = 逐步证据
--   （`[{action,result,op,ms,outPath,sha256,...}]`）。
CREATE TABLE IF NOT EXISTS asset_versions (
    versionId   INTEGER PRIMARY KEY AUTOINCREMENT,
    assetId     INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    jobId       INTEGER,
    runId       INTEGER,
    packId      TEXT    NOT NULL,
    packVersion TEXT    NOT NULL DEFAULT '',
    pluginHash  TEXT    NOT NULL DEFAULT '',
    actions     TEXT    NOT NULL,
    result      TEXT    NOT NULL CHECK (result IN ('ok','failed')),
    errorCode   TEXT,
    errorMessage TEXT,
    path        TEXT,
    sha256      TEXT,
    bytes       INTEGER,
    format      TEXT,
    width       INTEGER,
    height      INTEGER,
    assetName   TEXT,
    chain       TEXT    NOT NULL,
    durationMs  INTEGER NOT NULL DEFAULT 0,
    createdAt   INTEGER NOT NULL,
    current     INTEGER NOT NULL DEFAULT 0 CHECK (current IN (0,1))
);

-- 「只有一张工作版本」是硬约束：同资产最多一行 current=1
CREATE UNIQUE INDEX IF NOT EXISTS uq_asset_versions_current
    ON asset_versions(assetId) WHERE current = 1;
CREATE INDEX IF NOT EXISTS idx_asset_versions_asset ON asset_versions(assetId, createdAt DESC);
CREATE INDEX IF NOT EXISTS idx_asset_versions_job ON asset_versions(jobId);
