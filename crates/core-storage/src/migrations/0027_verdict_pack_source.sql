-- 0027 verdict_pack_source — IP-K05（卡 task）。
--
-- 判定行的**插件来源列**：谁下的这个结论。
--
-- 历史行为：一条判定只能反推「哪个 pack（packId）」+「哪条规则（ruleId）」，但同一 pack
-- 有两条执行通道（legacy 内联引擎 / plugin 子进程插件），同一 id 的包还会升级版本 ——
-- 出争议时「是用哪份包代码判的」无从回答。本迁移补两列：
--
-- * `packVersion` = 产出该行的包版本（legacy 通道 = 运行包自身版本；plugin 通道 = 插件
--   manifest.version）。历史行留 NULL（不可知就不造数，与 0016 的 confidence 同口径）。
-- * `pluginHash` = 产出该行的包代码身份 = `manifest.json` 原文 sha256（= `packs.manifest_hash`）。
--   legacy 通道留 NULL（不是插件产物，没有代码身份）；plugin 通道必填。
--
-- 两列都允许 NULL 是**迁移兼容**，不是「可以不给」：0027 之后由插件通道写入的行两列必填，
-- 见 `core_storage::compliance::NewVerdict` 的写入路径与 svc-pipeline 的映射。
ALTER TABLE rule_verdict ADD COLUMN packVersion TEXT;
ALTER TABLE rule_verdict ADD COLUMN pluginHash TEXT;

-- 定位「某份包代码影响过哪些判定」（争议回溯/重判范围计算）
CREATE INDEX IF NOT EXISTS idx_rule_verdict_plugin_hash
    ON rule_verdict(pluginHash) WHERE pluginHash IS NOT NULL;
