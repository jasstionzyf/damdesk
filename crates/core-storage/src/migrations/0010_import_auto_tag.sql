-- 0010 import autoTag — W6 修复（task）：导入批次记录「导入后自动打标」选项，
-- 断点续跑（/api/import/resume）据此恢复同一行为，不再依赖进程内存。

ALTER TABLE import_batches ADD COLUMN autoTag INTEGER NOT NULL DEFAULT 0;
