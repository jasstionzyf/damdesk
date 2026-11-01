-- 0009 tag_review — E5（打标交互式确认流）：AI 打标结果两段式落库。
-- 迁移纪律「只加不改不删」：纯加一张新表。
-- tag_proposal 是打标结果的暂存区（不直接写 assets/tag 表）：
-- * kind='tag'          → 标签提案（name=标签名；value=自定义维度值，可空；taskId=自定义任务溯源）
-- * kind='title'        → title_ch 提案（value=提案文本）
-- * kind='titleEn'      → title_en 提案
-- * kind='description'  → description 提案
-- score=模型置信度（前端 <0.6 黄色高亮）；edited=1 表示人工在确认页改过/新增，
-- 提交时按人工数据落库（origin='manual' / 字段覆写），未经修改的按原语义（auto/custom、只填空字段）。
-- 逐资产整批生效：确认（review_commit）→ 搬进正式表并删提案；跳过（review_discard）→ 只删提案，原图不动。

CREATE TABLE tag_proposal (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    kind      TEXT    NOT NULL DEFAULT 'tag' CHECK (kind IN ('tag','title','titleEn','description')),
    name      TEXT    NOT NULL DEFAULT '',
    value     TEXT    NOT NULL DEFAULT '',
    model     TEXT,
    taskId    INTEGER REFERENCES custom_tag_task(id) ON DELETE CASCADE,
    score     REAL    NOT NULL DEFAULT 0.9,
    edited    INTEGER NOT NULL DEFAULT 0 CHECK (edited IN (0,1)),
    createdAt INTEGER NOT NULL
);
CREATE INDEX idx_tag_proposal_asset ON tag_proposal(assetId);
CREATE INDEX idx_tag_proposal_task ON tag_proposal(taskId);
-- 同资产同维度唯一：字段类提案每资产一行（name=''），标签提案同名一行（重跑整批替换在先，
-- 唯一索引兜底重复新增）。
CREATE UNIQUE INDEX uq_tag_proposal ON tag_proposal(assetId, kind, name);
