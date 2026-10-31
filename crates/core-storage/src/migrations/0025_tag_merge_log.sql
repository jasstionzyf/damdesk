-- 0025（task 标签总览）：合并/改名/删除的审计留痕。
--
-- 裁决（plan 2026-10-20_113449 §6）：**不提供撤销**，只留痕。所以这张表只记「谁在什么时候
-- 把哪些值合到哪个值、影响多少资产」——不是回收站，不做 30 天恢复，也不参与任何读路径。
--
-- 口径：
-- * `kind` = 'merge'（含 rename：rename 是单源 merge）/ 'delete'（只允许 count=0 的值）；
-- * `affected` = 受影响资产数（去重、仅 deleted=0）；delete 记 0（本来没有在库资产）；
-- * `removed` = 被合并掉/被删掉的值清单（JSON 数组文本，一条日志对应一次用户动作）；
-- * `toValue` = 保留值；delete 记空串（没有目标值）；
-- * `operator` = 操作者标识（桌面单用户产品，固定写 'local'，留字段给多用户形态）。
CREATE TABLE IF NOT EXISTS tag_merge_log (
    id        INTEGER PRIMARY KEY,
    ts        INTEGER NOT NULL,
    kind      TEXT    NOT NULL CHECK (kind IN ('merge','delete')),
    fromValue TEXT    NOT NULL,          -- 源值（一动作多源时逐行写，便于按值追溯）
    toValue   TEXT    NOT NULL DEFAULT '',
    affected  INTEGER NOT NULL DEFAULT 0,
    removed   TEXT    NOT NULL DEFAULT '[]',
    operator  TEXT    NOT NULL DEFAULT 'local'
);
CREATE INDEX IF NOT EXISTS idx_tag_merge_log_ts ON tag_merge_log(ts);
CREATE INDEX IF NOT EXISTS idx_tag_merge_log_from ON tag_merge_log(fromValue);
