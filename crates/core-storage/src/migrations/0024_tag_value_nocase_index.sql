-- 0024（task 标签匹配语义）：tag.value 的 NOCASE 前导索引。
-- 两条消费路径：
-- ① 过滤（svc-library::filter_where / engine-search::candidate_ids 的
--    `WHERE value = ?N COLLATE NOCASE`）——大小写不敏感命中必须走 nocase 索引，
--    否则 SQLite 拿 BINARY 的 idx_tag_value 对不上（collation 不匹配 → 全表扫）。
-- ② 候选提示（svc-library::tag_candidates 的 `WHERE value LIKE 'prefix%' ESCAPE '\'`）
--    —— 默认 case_sensitive_like=OFF 时，LIKE 前缀优化只认 NOCASE 索引。
-- 只加不改不删；与 0023 的 BINARY 索引并存（GROUP BY value / 精确原值匹配继续走它）。
CREATE INDEX IF NOT EXISTS idx_tag_value_nocase ON tag(value COLLATE NOCASE);
