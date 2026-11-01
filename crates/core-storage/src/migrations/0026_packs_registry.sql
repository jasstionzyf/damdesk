-- 0026 packs_registry — IP-K05（卡 t_06c17b35）。
--
-- 行业包**身份账本**：`manifest.json` 是唯一配置面（协议 §4.0），本表只是它的落库快照，
-- 用来回答「这张图的判定是哪个包、哪个版本、哪份 manifest（hash）跑出来的」——
-- verdict 行的 `packVersion`/`pluginHash`（0027）靠它解码。
--
-- 口径（勿改）：
-- * 主键 = `(id, version)`：同 `id` 多版本**并存**（协议 §4.1「回滚 = 切指针」），
--   所以不能只按 id 唯一；升级不删旧行（审计要能对上历史判定的版本）。
-- * `manifest_hash` = `manifest.json` 原文 sha256（`pack_host::PackIdentity::plugin_hash`），
--   与判定行里的 `pluginHash` 逐字相同 —— 该值变了就说明包内容变了，不是同一个包。
-- * `dir` = 包目录绝对路径（安装/内置路径），排障时用来定位实物。
-- * `signature_status`：`unsigned`（开发期 manifest.signature 为 null）|
--   `declared`（manifest 带签名字符串但未校验）| `verified` / `invalid`（校验过的结论）。
--   v1 只写 `unsigned`/`declared`；`verified`/`invalid` 留给签名校验实现（M1 硬验收之后）。
-- * `current` = 该 `id` 当前生效版本（唯一索引兜底，同一时刻一个 id 只能有一个 current=1）。
--   「切指针」= 先把该 id 全部置 0，再置目标版本 1（事务内完成）。
-- * `installed_at` = 首次登记时间（unix 秒，可移植类型）；重复登记只刷新可见字段，不改它。
CREATE TABLE IF NOT EXISTS packs (
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
    PRIMARY KEY (id, version)
);

-- 一个 id 只能有一个生效版本（切指针竞态由 DB 兜底，不靠应用层自觉）
CREATE UNIQUE INDEX IF NOT EXISTS idx_packs_current
    ON packs(id) WHERE current = 1;
