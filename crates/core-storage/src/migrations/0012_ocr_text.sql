-- 0012 ocr_text — OCR-E：PP-OCRv6_medium 全文行文本落列 + 文本检索加权融合数据源。
-- 迁移纪律（PLAN.md §3）：只加不改不删。

-- 识别文本：行文本按 '\n' 连接（阅读序）；'' = 未识别。检索侧 LIKE 加权融合
-- （svc-library::fuse_ocr_text，纯 SQL，web 容器同样生效）。
ALTER TABLE assets ADD COLUMN ocr_text TEXT NOT NULL DEFAULT '';

-- 排队语义锚点：job_queue(kind='ocr') 入队条件 = 图片 & ocr_text = ''
CREATE INDEX idx_assets_ocr_empty ON assets(assetId) WHERE ocr_text = '' AND deleted = 0;
