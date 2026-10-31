-- 0002 import_batches — 导入批次/断点（M1 导入器使用；D1 先建表）

CREATE TABLE import_batches (
    batchId    INTEGER PRIMARY KEY,
    rootPath   TEXT    NOT NULL,
    storeMode  TEXT    NOT NULL DEFAULT 'reference' CHECK (storeMode IN ('reference','copyin')),
    status     TEXT    NOT NULL DEFAULT 'running' CHECK (status IN ('running','done','failed','cancelled')),
    totalFiles INTEGER NOT NULL DEFAULT 0,
    doneFiles  INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    startedAt  INTEGER NOT NULL,
    endedAt    INTEGER
);
