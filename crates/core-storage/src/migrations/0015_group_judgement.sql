-- 0015 group_judgement — C9（按组判定，卡 t_ff666f9c）：组级判定的**落点**。
-- 迁移纪律：只加不改不删 —— 旧库（0014 = ocr_fts 形态）升级只是两条 ADD COLUMN + 一条索引，
-- 不动既有行、不重建表（rule_verdict 历史判定全保留）。
--
-- 背景（C7 房产包 spike 缺口 G1，最大缺口）：组级判定原先无落点 ——
-- `rule_verdict.assetId NOT NULL` 且无组键列、pack 无 judgement_scope 概念 →
-- 「这套房源照片齐不齐 / 同组有几张重复房照」只能逐图结论 + 外部自己聚合。
--
-- judgementScope：`per_asset` | `per_group`（NULL = 0014 之前的历史行，语义等价 per_asset）。
--   per_group 判定按**成员展开**落库：同一组同一条规则，组内每个成员一行，四列内容
--   （result/score/evidence/fixAction…）逐字相同。展开的理由：库视图状态筛选
--   （`state_condition_sql` 的 `v.assetId = a.assetId`）、报告导出、人工反馈**都按 assetId 取行**；
--   展开后这些读路径零改动即可满足「组内成员可查」（组级结论对组内每个成员都成立）。
--   一组的判定只**执行**一次（组级 job，见 svc-pipeline `compliance_group`），
--   落库是 N 行 —— 「执行次数 = 组数，落库行数 = 组成员数」。
-- groupKey：组键取值（`source: folder` → 目录绝对路径；`source: asset_field, field: X` → assets.X 取值）。
--   缺键（取值空/解析不到）→ 该成员单独成组，落 `pending_human` + `evidence.reason=group_key_missing`
--   （响亮，不静默当合格）。
--   job_queue 无需改表：组级 job 用新 kind `compliance_group`，targetId = 组锚点 assetId，
--   组目标（groupKey + 成员 assetId 列表 + group_by 作用域）载在 payload —— kind/targetId/payload
--   三元组本就足以表达组目标，加列反而多一处真相。

ALTER TABLE rule_verdict ADD COLUMN judgementScope TEXT
    CHECK (judgementScope IS NULL OR judgementScope IN ('per_asset','per_group'));
ALTER TABLE rule_verdict ADD COLUMN groupKey TEXT;
-- 「查某组某规则的全部成员行」= 组内成员可查（库视图/报告/审计）
CREATE INDEX idx_rule_verdict_group ON rule_verdict(packId, ruleId, groupKey);
