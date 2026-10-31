-- 0006 path_tag_origin — D8（裁决 2026-10-13）：tag.origin 扩展 'path'
-- （目录名/文件名解析标签，与 auto/manual 共存不合并）。
-- SQLite CHECK 约束不可 ALTER，按标准重建流程换表：数据原样搬迁、索引同名重建，
-- 语义上仅放宽 origin 取值集合（迁移纪律「只加不改不删」在此处的唯一可行等价物）。

CREATE TABLE tag_new (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    name      TEXT    NOT NULL,
    origin    TEXT    NOT NULL DEFAULT 'auto' CHECK (origin IN ('auto','manual','path')),
    model     TEXT,
    score     REAL    NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL
);
INSERT INTO tag_new (id, assetId, name, origin, model, score, createdAt)
    SELECT id, assetId, name, origin, model, score, createdAt FROM tag;
DROP TABLE tag;
ALTER TABLE tag_new RENAME TO tag;
CREATE UNIQUE INDEX uq_tag_asset_name ON tag(assetId, name);
CREATE INDEX idx_tag_name ON tag(name);
