-- 0031 spec_plugin_ids — N18（卡 task）。
--
-- 背景：N1 规格落库时，「无代码插件的能力」允许 `plugin_spec.pluginId` 写**配置包 id**
-- （`ecommerce-taobao` / `realestate`）。IP-K16 删了内置/导入的 YAML 行业包之后那些 id
-- 不再对应任何可装载对象 → 规格在判定入口会被判「插件没装」，用户看到的是一条死配置。
--
-- 本迁移把它们改键到**插件身份 id**（插件由 `.dampack` 安装，装了就立刻可用）：
--   ecommerce-taobao → com.soujpg.pack.ecommerce
--   realestate       → com.soujpg.pack.realestate
--
-- 幂等：按 id 字面匹配，跑第二遍命中 0 行。改键后再撞唯一索引（pluginId+name）的行
-- 不改（保留用户既有同名规格，冲突的那条留给用户决定 —— 迁移不做删除）。
--
-- 说明：`originKey`（`legacy-pack:<id>`）**故意不动** —— 它记录「这条规格是从哪个旧包迁来的」，
-- 是历史事实；幂等/追溯都靠它。

UPDATE plugin_spec
   SET pluginId = 'com.soujpg.pack.ecommerce'
 WHERE pluginId IN ('ecommerce-taobao', 'ecommerce')
   AND NOT EXISTS (
       SELECT 1 FROM plugin_spec x
        WHERE x.pluginId = 'com.soujpg.pack.ecommerce'
          AND x.name = plugin_spec.name
   );

UPDATE plugin_spec
   SET pluginId = 'com.soujpg.pack.realestate'
 WHERE pluginId IN ('realestate')
   AND NOT EXISTS (
       SELECT 1 FROM plugin_spec x
        WHERE x.pluginId = 'com.soujpg.pack.realestate'
          AND x.name = plugin_spec.name
   );
