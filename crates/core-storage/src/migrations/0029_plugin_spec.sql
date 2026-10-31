-- 0028 plugin_spec — N1 规格实体（卡 task）。
--
-- 口径锚点：damdesk-product `references/plugin-design-and-ui.md` §2/§5/§9、
-- `plugin-param-schema.md` §9。要点先钉死（后续别重议）：
--
-- * **规格不是插件、也不安装**：它只是「要做到什么标准」的一份配置
--   （选哪些规则 + 阈值 + 动作链 + 输出 + 参数 diff）。同一插件下可以有多条
--   （淘宝主图 / 亚马逊主图 / 我的 800px webp），互不影响。
-- * **默认值唯一定义在插件 resources**：本表 `paramsDiff` 只存用户改过的键
--   （diff），插件升级默认值时旧规格不会被锁死。
-- * **插件卸载/升级不动本表**：规格是用户资产。包没了 → 读接口把
--   `pluginStatus` 标成 `plugin_missing` / `version_mismatch`，**不静默消失**
--   （静默消失等于把用户的配置吃掉）；历史 verdict 与产物同样不删。
-- * **迁移来源可追溯**：`originKey` 记「这条规格是哪个旧行业包迁出来的」
--   （`legacy-pack:ecommerce-taobao` 等），唯一索引保证迁移**幂等**
--   （跑第二遍不会长出第二条淘宝主图）。`originKey` 为 NULL = 用户自建。
--
-- 列语义：
-- * `pluginId` = 该规格归属的能力身份。有代码插件时 = 插件 manifest id
--   （`com.soujpg.pack.ecommerce`）；无插件实现的老配置包 = 配置包 id
--   （如 `realestate`）。两者不要混用 —— 判定行的 packId 命名空间不同。
-- * `pluginVersionPin` = 规格绑定的插件版本；NULL = 跟随当前生效版本。
--   非 NULL 且与已装版本不同 → 读接口标 `version_mismatch`（不自动改写规格）。
-- * `rules` = 选哪些规则（JSON 数组，规则 id；空数组 = 插件全部规则）。
-- * `thresholds` = **规则级阈值**（JSON 对象：`{ruleId: {key: value}}`），
--   旧行业包 YAML 的阈值原样落这里 —— 它是「阈值不丢」的物证列，
--   且插件侧本来就认 `options.thresholds` 这个结构（宿主不做业务映射）。
-- * `actions` = 处理类动作链（JSON 数组，有序；元素 `{action, params?, when?}`）。
-- * `output` = 输出设置（JSON 对象：格式/尺寸/命名等）。
-- * `paramsDiff` = 插件 schema 键的参数 diff（JSON 对象；只存用户改过的键）。
--   与 `thresholds` 并存不是双写：前者是插件声明面（宿主按 schema 校验），
--   后者是规则面（迁移自旧 YAML 的原文保留，插件自己折成阈值）。
-- * `enabled` = 规格启停（0 = 停用：仍可见可编辑，但不许被任务引用）。

CREATE TABLE IF NOT EXISTS plugin_spec (
    specId           INTEGER PRIMARY KEY AUTOINCREMENT,
    pluginId         TEXT    NOT NULL,
    pluginVersionPin TEXT,
    name             TEXT    NOT NULL,
    industry         TEXT,
    rules            TEXT    NOT NULL DEFAULT '[]',
    thresholds       TEXT    NOT NULL DEFAULT '{}',
    actions          TEXT    NOT NULL DEFAULT '[]',
    output           TEXT    NOT NULL DEFAULT '{}',
    paramsDiff       TEXT    NOT NULL DEFAULT '{}',
    enabled          INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    originKey        TEXT,
    createdAt        INTEGER NOT NULL DEFAULT (unixepoch()),
    updatedAt        INTEGER NOT NULL DEFAULT (unixepoch())
);

-- 同一插件下规格名唯一（用户心智：淘宝主图 / 亚马逊主图，不是三个同名条目）
CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_spec_name
    ON plugin_spec(pluginId, name);

-- 迁移幂等：同一个旧行业包只能迁出一条规格（用户自建 originKey 为 NULL，不受约束）
CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_spec_origin
    ON plugin_spec(originKey) WHERE originKey IS NOT NULL;

-- 按插件列规格（插件详情页「我的规格」段的唯一查询路径）
CREATE INDEX IF NOT EXISTS idx_plugin_spec_plugin
    ON plugin_spec(pluginId, specId);
