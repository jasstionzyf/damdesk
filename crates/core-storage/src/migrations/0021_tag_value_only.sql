-- 0021 tag_value_only — 打标数据模型纠正（2026-10-19 裁决）：
-- 标签就是标签：`tag` 只存标签值（value），不再有 name 维度列；
-- 打标任务的结果落独立结果表 `tag_result`（name = 任务维度名，value = 模型输出），
-- 结果 value 非空时自动扇出到 `tag`（origin='custom' + model + taskId 溯源）。
--
-- 迁移纪律等价物：`tag` 仅做「name 语义 → value 语义」的等价搬迁（换表 + 同名索引重建），
-- custom 行的维度名搬到新表 tag_result，其余来源的 name 值原样变成 tag.value。
-- 既有 custom 值按「;」「；」换行拆成多个标签（结果表保留原始整串）。

CREATE TABLE tag_result (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    taskId    INTEGER REFERENCES custom_tag_task(id) ON DELETE SET NULL,
    name      TEXT    NOT NULL,              -- 维度名快照（custom_tag_task.name）
    value     TEXT    NOT NULL,              -- 模型原始输出（可能多值，未拆分）
    model     TEXT,
    score     REAL    NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL,
    updatedAt INTEGER NOT NULL
);
CREATE UNIQUE INDEX uq_tag_result_asset_task ON tag_result(assetId, taskId);
CREATE INDEX idx_tag_result_task ON tag_result(taskId);
CREATE INDEX idx_tag_result_name ON tag_result(name);

-- 1) custom 行 → 结果表（同资产同任务多行时取最新一行）
INSERT OR IGNORE INTO tag_result (assetId, taskId, name, value, model, score, createdAt, updatedAt)
SELECT assetId, taskId, name, value, model, score, createdAt, createdAt FROM (
    SELECT assetId, taskId, name, TRIM(value) AS value, model, score, createdAt,
           ROW_NUMBER() OVER (PARTITION BY assetId, taskId ORDER BY id DESC) AS rn
    FROM tag
    WHERE origin = 'custom' AND value IS NOT NULL AND TRIM(value) <> ''
) WHERE rn = 1;

-- 2) tag 重建为「仅 value」：custom 用 value，其余来源用原 name
CREATE TABLE tag_new (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    value     TEXT    NOT NULL,
    origin    TEXT    NOT NULL DEFAULT 'auto' CHECK (origin IN ('auto','manual','path','custom')),
    model     TEXT,
    taskId    INTEGER REFERENCES custom_tag_task(id) ON DELETE SET NULL,
    score     REAL    NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL
);
CREATE UNIQUE INDEX uq_tag_asset_value ON tag_new(assetId, value);

-- 拆分：统一分隔符为 ';'（；/换行先归一），递归切分后按来源优先级去重
WITH RECURSIVE norm AS (
    SELECT id, assetId, origin, model, taskId, score, createdAt,
           REPLACE(REPLACE(REPLACE(
               CASE WHEN origin = 'custom' THEN TRIM(COALESCE(value, '')) ELSE TRIM(name) END,
               '；', ';'), char(10), ';'), char(13), ';') AS s,
           CASE origin WHEN 'manual' THEN 0 WHEN 'custom' THEN 1 WHEN 'path' THEN 2 ELSE 3 END AS pri
    FROM tag
),
walk(assetId, origin, model, taskId, score, createdAt, pri, rest) AS (
    SELECT assetId, origin, model, taskId, score, createdAt, pri, s || ';' FROM norm WHERE s <> ''
    UNION ALL
    SELECT assetId, origin, model, taskId, score, createdAt, pri, substr(rest, instr(rest, ';') + 1)
    FROM walk WHERE instr(rest, ';') > 0
)
INSERT OR IGNORE INTO tag_new (assetId, value, origin, model, taskId, score, createdAt)
SELECT assetId, TRIM(substr(rest, 1, instr(rest, ';') - 1)), origin, model, taskId, score, createdAt
FROM walk
WHERE TRIM(substr(rest, 1, instr(rest, ';') - 1)) <> ''
ORDER BY pri, assetId, createdAt;

DROP TABLE tag;
ALTER TABLE tag_new RENAME TO tag;
CREATE INDEX idx_tag_value ON tag(value);
CREATE INDEX idx_tag_task ON tag(taskId);
