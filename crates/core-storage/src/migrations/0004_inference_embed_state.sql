-- 0004 inference_embed_state — D3 推理接入层：assets 嵌入状态机字段 + inference 设置占位
-- 迁移纪律（PLAN.md §3）：只加不改不删。

-- emb_status 状态机：none(未处理) → pending(排队) → done(向量已产出) → failed(可重试)
ALTER TABLE assets ADD COLUMN emb_status TEXT NOT NULL DEFAULT 'none'
    CHECK (emb_status IN ('none','pending','done','failed'));
-- emb_model = provider spec.name（禁混算铁律：向量表按 name 分表，PLAN.md §9）
ALTER TABLE assets ADD COLUMN emb_model TEXT;
ALTER TABLE assets ADD COLUMN emb_at INTEGER;

CREATE INDEX idx_assets_embstatus ON assets(emb_status, emb_model);

-- 推理设置占位（真实值来自 core-config::InferenceSettings 默认值，用户改动才写库）
INSERT OR IGNORE INTO settings (key, value, updatedAt) VALUES
  ('inference', 'null', unixepoch());
