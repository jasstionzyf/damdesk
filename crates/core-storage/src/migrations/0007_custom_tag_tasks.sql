-- 0007 custom_tag_tasks — D10（裁决 2026-10-13）：自定义打标任务 + tag 表扩展。
-- 迁移纪律「只加不改不删」：
-- * custom_tag_task 为新表（纯加）：name 唯一（= tag.name 维度名）。
-- * tag.value（推理输出短文本）/ tag.taskId（任务溯源）为加列（可空，旧行 NULL）。
-- * origin 取值集合扩展 'custom' 需放宽 CHECK——SQLite CHECK 不可 ALTER，
--   沿用 0006 的标准重建流程换表：数据原样搬迁、索引同名重建，
--   语义上仅放宽 origin 集合 + 新增可空列（「只加」的唯一可行等价物）。

CREATE TABLE custom_tag_task (
    id        INTEGER PRIMARY KEY,
    name      TEXT    NOT NULL UNIQUE,
    prompt    TEXT    NOT NULL,
    enabled   INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    createdAt INTEGER NOT NULL,
    updatedAt INTEGER NOT NULL
);

CREATE TABLE tag_new (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    name      TEXT    NOT NULL,
    value     TEXT,
    origin    TEXT    NOT NULL DEFAULT 'auto' CHECK (origin IN ('auto','manual','path','custom')),
    model     TEXT,
    taskId    INTEGER REFERENCES custom_tag_task(id) ON DELETE SET NULL,
    score     REAL    NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL
);
INSERT INTO tag_new (id, assetId, name, value, origin, model, taskId, score, createdAt)
    SELECT id, assetId, name, NULL, origin, model, NULL, score, createdAt FROM tag;
DROP TABLE tag;
ALTER TABLE tag_new RENAME TO tag;
CREATE UNIQUE INDEX uq_tag_asset_name ON tag(assetId, name);
CREATE INDEX idx_tag_name ON tag(name);
CREATE INDEX idx_tag_task ON tag(taskId);
