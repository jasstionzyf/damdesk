-- 0019 dup_edges — KI-2b（卡 task）：向量判重的**关系表**。
--
-- 背景：线上「去重」只有 fileHash + pHash 两套判据；DINOv3 判重塔的向量此前只落
-- `v_<model>` 虚表，没有任何「哪两张图是同一视觉内容」的持久证据 —— 去重页看不到视觉
-- 判重结果，重启后也无从复现（只能每次进页面重算全库 KNN）。
--
-- 为什么是关系表，而不是「查的时候现算」：
-- 1. 向量只回答「查询图离库内图多远」，回答不了「库内两两已成组」；每次进页面重算
--    是 O(n·k) 的反复开销，且结果不可审计、不可复现；
-- 2. 边是**幂等可重算**的派生数据：判一个资产 = 删掉它涉及的旧边 + 按当前向量写新边
--    （svc_pipeline::embed_queue::judge_asset_dup_edges），重嵌入/换邻居集后自然自愈，
--    不会留下「已经不相似了边还在」的历史结论；
-- 3. 分组时把「边」与老的 fileHash/pHash 判据一起做并查集：两套判据**分层合并展示、
--    kind 分开标注**（不把视觉近似混进「精确重复」概念里）。
--
-- 语义要点：
-- * `a < b` 强制规范化（无向边只存一份，CHECK 兜底），主键 (a,b) 天然幂等；
-- * `score` = 余弦相似度（= 1 - vec0 distance）；`kind` 由写入方按阈值定档：
--   near_visual（≥0.98 同一视觉内容）/ variant（[0.90,0.98) 同图不同构图）；
-- * `model` = 判重塔模型名（core_config::DEDUP_EMBEDDER）：换模型后旧边可辨，
--   不被误当作新模型的结论；
-- * 只存 assetId（不存 resourceId）：分组/裁决/回收站都以资产为粒度，
--   resource 只是向量的挂载点；资产被彻底删时边随 CASCADE 一起清掉。

CREATE TABLE dup_edges (
    a         INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    b         INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    score     REAL    NOT NULL CHECK (score >= -1.0 AND score <= 1.0),
    kind      TEXT    NOT NULL CHECK (kind IN ('near_visual','variant')),
    model     TEXT    NOT NULL,
    createdAt INTEGER NOT NULL,
    PRIMARY KEY (a, b),
    CHECK (a < b)
);

-- 反向查（「这张图参与的所有边」）——判重前先删后写走 b 列
CREATE INDEX idx_dup_edges_b ON dup_edges(b);

-- 判重扫描账本：存量一次性补判的可重入状态（backfill_done / backfill_truncated /
-- last_judge_at）。没有它，UI 分不清「没有重复」与「还没检测」（KI-2 口径：
-- 找不到向量时不得渲染成「无重复」）。
CREATE TABLE dup_judge_state (
    key       TEXT    PRIMARY KEY,
    value     TEXT    NOT NULL,
    updatedAt INTEGER NOT NULL
);
