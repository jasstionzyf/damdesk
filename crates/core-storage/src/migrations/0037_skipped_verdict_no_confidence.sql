-- 0037 skipped_verdict_no_confidence — DEC-F2（卡 t_a4b7e0ee）。

-- 契约（t_9267ed7a 资产级口径 → 本卡落到规则行存储层）：`skipped` = 这条规则对这张图
-- **没有结论**，不携带分数与把握度 —— `result='skipped'` 的行 `confidence IS NULL`
-- 且 `score IS NULL`（NULL，不是 0，也不是 1.0）。

-- 写侧：`compliance_sqlite::insert_verdict` 的 skipped 拆列 CASE 已延伸到
-- confidence/score（0028 同一处的契约点，新行天然满足）。本迁移只回填存量行
-- —— 宿主两条写路径（domain_gate 主行 + short_circuit 行）在修复前把
-- confidence/score 写成了 1.0。

-- 幂等：UPDATE 谓词只命中 skipped 行、目标值恒为 NULL，跑两次结果一致；
-- 非 skipped 行（pass/fail/undetermined 的 confidence/score）一字不动。

UPDATE rule_verdict SET confidence = NULL, score = NULL WHERE result = 'skipped';
