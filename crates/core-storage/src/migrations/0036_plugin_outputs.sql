-- 0036 plugin_outputs — 协议 v2.0 契约改名（卡 t_84fbb36e）。
--
-- `packs.kinds` → `packs.outputs`：列改名 + 值域换词（judge→verdict / process→artifact）。
-- CHECK 约束里的闭集词变了，SQLite 不能改 CHECK → 按惯例**重建表**（同 0028 的做法）。
--
-- 值映射（旧缺省 'judge' 一并覆盖 —— v1.x 未声明 kinds 的包 = 判定类）：
--   'judge'         → 'verdict'
--   'process'       → 'artifact'
--   'judge,process' → 'verdict,artifact'
--
-- 幂等性由迁移框架保证（版本号一次性执行）；迁移体对「outputs 已存在」的库不成立
-- （旧表没有 outputs 列，重建语句只认 kinds —— 二次执行不会发生，也不需要成立）。
-- 审计面不动：manifest_hash / 判定行的 pluginHash 是历史事实，不随迁移改写。

CREATE TABLE packs_v2 (
    id               TEXT    NOT NULL,
    version          TEXT    NOT NULL,
    dir              TEXT    NOT NULL,
    manifest_hash    TEXT    NOT NULL,
    api_version      TEXT    NOT NULL,
    deterministic    INTEGER NOT NULL DEFAULT 0 CHECK (deterministic IN (0, 1)),
    signature_status TEXT    NOT NULL DEFAULT 'unsigned'
                             CHECK (signature_status IN ('unsigned','declared','verified','invalid')),
    installed_at     INTEGER NOT NULL,
    current          INTEGER NOT NULL DEFAULT 1 CHECK (current IN (0, 1)),
    -- 插件输出种类（manifest `outputs` 的规范化形态，v2.0 必填无缺省；本列的
    -- DEFAULT 'verdict' 只兜 0028 之前登记、没有 kinds 的历史行）。
    outputs          TEXT    NOT NULL DEFAULT 'verdict'
                             CHECK (outputs IN ('verdict','artifact','verdict,artifact')),
    PRIMARY KEY (id, version)
);

INSERT INTO packs_v2
       (id, version, dir, manifest_hash, api_version, deterministic,
        signature_status, installed_at, current, outputs)
SELECT id, version, dir, manifest_hash, api_version, deterministic,
       signature_status, installed_at, current,
       CASE kinds
           WHEN 'process'       THEN 'artifact'
           WHEN 'judge,process' THEN 'verdict,artifact'
           ELSE 'verdict'
       END
FROM packs;

DROP TABLE packs;
ALTER TABLE packs_v2 RENAME TO packs;

-- 一个 id 只能有一个生效版本（切指针竞态由 DB 兜底，不靠应用层自觉）
CREATE UNIQUE INDEX IF NOT EXISTS idx_packs_current
    ON packs(id) WHERE current = 1;
