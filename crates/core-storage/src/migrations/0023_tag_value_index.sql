-- 0023（t_d65d55a1 统一标签A）：tag.value 前导索引。
-- 统一标签后所有过滤/统计/交叉查询只走 tag.value（engine-search::candidate_ids 的
-- `assetId IN (SELECT assetId FROM tag WHERE value = ?N)`、标签云 GROUP BY value 等），
-- 既有 uq_tag_asset_value(assetId, value) 前导列是 assetId，对 WHERE value=? / GROUP BY value
-- 不走索引（现态子查询全表扫）。实测 750 万行（50 万资产 × ~15 标签）规模：
-- 走本索引后单值筛选 ~3ms、分布 top20 / 前缀分组 / 多标签交叉 ~220ms，单机 SQLite 可运营。
-- 只加不改不删；索引建失败（同名异构）直接报错终止迁移。
CREATE INDEX IF NOT EXISTS idx_tag_value ON tag(value);
