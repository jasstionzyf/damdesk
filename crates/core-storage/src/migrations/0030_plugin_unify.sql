-- 0028 plugin_unify — IP-K16（卡 task）。
--
-- 用户裁决（2026-10-22）：一个行业的合规检测/处理 = **一个插件**，插件自带全部信息
-- （规则 / 词表 / 报告元数据 / 流程代码）。宿主不再内置 YAML 行业包，也不再有两个
-- 执行通道（legacy 内联引擎 / plugin 子进程）——判定行的归属 id 因此统一到**插件身份**。
--
-- 本迁移做三件事（幂等，可重跑）：
--   1. 判定/反馈行的 `packId`：老的内置 YAML 包 id `ecommerce-taobao` → 插件 id
--      `com.soujpg.pack.ecommerce`（否则老判定在新读口径下"看不见了"= 假装没判过）；
--   2. 人工改判行同口径改键（活记录唯一索引兜底：与已有插件行撞键的老行先删，改判以插件行为准）；
--   3. 清掉「激活行业包 / 已导入行业包」这两个设置项 —— 对应的对象已删除，
--      留着只会让人以为「还得再装一套行业包规则」。
--
-- 说明：`packVersion` / `pluginHash`（0027）不动 —— 老行是 legacy 通道产物，
-- 没有插件代码身份，给 NULL 才是真话。

UPDATE rule_verdict SET packId = 'com.soujpg.pack.ecommerce' WHERE packId = 'ecommerce-taobao';

UPDATE rule_feedback SET packId = 'com.soujpg.pack.ecommerce' WHERE packId = 'ecommerce-taobao';

-- 活记录（revokedAt IS NULL）唯一：先删「迁移后会撞唯一键」的老行，再整体改键
DELETE FROM verdict_human_review
 WHERE packId = 'ecommerce-taobao'
   AND revokedAt IS NULL
   AND EXISTS (
       SELECT 1 FROM verdict_human_review x
        WHERE x.packId = 'com.soujpg.pack.ecommerce'
          AND x.revokedAt IS NULL
          AND x.assetId = verdict_human_review.assetId
          AND COALESCE(x.ruleId, '*') = COALESCE(verdict_human_review.ruleId, '*')
   );

UPDATE verdict_human_review SET packId = 'com.soujpg.pack.ecommerce' WHERE packId = 'ecommerce-taobao';

DELETE FROM settings WHERE key IN ('compliance.activePack', 'compliance.importedPacks', 'compliance');
