-- 0005 dedup_vectors — D5 双嵌入器裁决（2026-10-13）：
--   siglip2（语义检索，emb_status 继续）+ dinov3（去重，dedup_status 新增），两队列互不阻塞。
-- 迁移纪律（PLAN.md §3）：只加不改不删。

-- 去重向量进度状态机（语义侧对称；'running' 为 job_queue 运行态，不落资产列，
-- 与 emb_status 的 none/pending/done/failed 口径一致）
ALTER TABLE assets ADD COLUMN dedup_status TEXT NOT NULL DEFAULT 'none'
    CHECK (dedup_status IN ('none','pending','done','failed'));
ALTER TABLE assets ADD COLUMN dedup_model TEXT;
ALTER TABLE assets ADD COLUMN dedup_at INTEGER;

CREATE INDEX idx_assets_dedupstatus ON assets(dedup_status, dedup_model);

-- 向量表登记簿：v_<sanitized model>（vec0 虚表由 core-storage::vecstore 动态建）。
-- 禁混算铁律：表按 embModel 分表，跨表不可比（PLAN.md §9）。
CREATE TABLE vector_tables (
    model     TEXT PRIMARY KEY,
    dims      INTEGER NOT NULL,
    tableName TEXT NOT NULL UNIQUE,
    createdAt INTEGER NOT NULL
);

-- 库目录设置（2026-10-13 裁决：libraryRoot 与 appData 分离；占位 null = 默认 {appData}/library）
INSERT OR IGNORE INTO settings (key, value, updatedAt) VALUES
  ('library', 'null', unixepoch());
