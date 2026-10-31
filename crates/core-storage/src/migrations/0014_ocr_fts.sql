-- 0014 ocr_fts — OCR-MATCH：`assets.ocr_text` 字面检索下推 FTS5 倒排（中文 2-gram
-- + 单字允许），取代 `ocr_text LIKE '%词%'` 全表扫描。迁移纪律（PLAN.md §3）：只加不改不删。
--
-- 分段（切词）在 Rust：core-storage::ocr_index::segment / query_terms（单一真源，
-- 查询侧 svc-library::fuse_ocr_text 共用同一函数）。SQLite 无字符类别函数，在 SQL 里
-- 复刻中文/拉丁口径必然与查询侧漂移，故触发器不自算分词，只做「同步保证」：
--   ① 任何 ocr_text 变更先按 rowid 撤掉旧索引行 → 陈旧命中不可能残留（哪怕写绕过应用）；
--   ② 写 ocr_fts_dirty 标记 → 由 ocr_index::sync_if_dirty（core_storage::open 每次探测）
--      用同一分段函数补建，索引缺失/写路径漏建都会自愈。
-- rowid = assetId：删除/覆盖 O(1)，不需要在虚表上扫非索引列（assetId 列式存法会把
-- 每次单行更新退化成全表扫描）。

CREATE VIRTUAL TABLE ocr_fts USING fts5(grams, tokenize = 'unicode61');

-- 待重建队列（触发器写入；写路径 upsert 成功后清除）
CREATE TABLE ocr_fts_dirty (
  assetId INTEGER PRIMARY KEY,
  ts      INTEGER NOT NULL
);

-- 迁移账本（segment_v 与 ocr_index::SEGMENT_VERSION 不一致 → 全库重标、按新口径重建）
CREATE TABLE ocr_fts_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
INSERT OR REPLACE INTO ocr_fts_state (key, value) VALUES ('segment_v', '1');

-- ① 新增（导入若带文本）= 待建
CREATE TRIGGER ocr_fts_ai AFTER INSERT ON assets
WHEN new.ocr_text <> '' AND new.deleted = 0
BEGIN
  INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts) VALUES (new.assetId, unixepoch());
END;

-- ② 文本变更：旧索引行当场撤掉 + 重标（写路径随后 upsert 并清标记）
CREATE TRIGGER ocr_fts_au AFTER UPDATE OF ocr_text ON assets
WHEN old.ocr_text <> new.ocr_text
BEGIN
  DELETE FROM ocr_fts WHERE rowid = old.assetId;
  INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts) VALUES (new.assetId, unixepoch());
END;

-- ③ 软删除（deleted 翻转）：索引只保留「未删且带文本」的资产 —— 查询侧因此不必再联
--    assets 过滤（那会让 SQLite 以 assets 驱动、逐行重算 rank：20k 库实测 1148ms）。
--    恢复（deleted 回到 0）时标 dirty，由 sync 重新建索引。
CREATE TRIGGER ocr_fts_aud AFTER UPDATE OF deleted ON assets
WHEN old.deleted <> new.deleted
BEGIN
  DELETE FROM ocr_fts WHERE rowid = old.assetId;
  INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts) VALUES (new.assetId, unixepoch());
END;

-- ④ 物理删除：索引行与待建标记都清
CREATE TRIGGER ocr_fts_ad AFTER DELETE ON assets
BEGIN
  DELETE FROM ocr_fts WHERE rowid = old.assetId;
  DELETE FROM ocr_fts_dirty WHERE assetId = old.assetId;
END;

-- 既有数据（0012 已落 ocr_text 的资产）一次性入待建队列，由 Rust 侧分段回填
INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts)
SELECT assetId, unixepoch() FROM assets WHERE ocr_text <> '' AND deleted = 0;
