-- 0008 smart_folders — E3（智能文件夹/保存搜索）：把一组筛选条件存为侧栏持久入口。
-- 迁移纪律「只加不改不删」：纯加一张新表。
-- query 存 AssetQuery 形态的 JSON（tag/dirPrefix/minRating/colorLabel/untagged/
-- mediaType/ext），分页参数（limit/offset）不属于保存条件，查看时由前端补。
-- 结果不落快照：每次进入/库变化都按条件现查（智能文件夹语义 = 动态视图）。

CREATE TABLE smart_folder (
    id        INTEGER PRIMARY KEY,
    name      TEXT    NOT NULL UNIQUE,
    query     TEXT    NOT NULL,
    createdAt INTEGER NOT NULL,
    updatedAt INTEGER NOT NULL
);
