-- 0001 init_core_tables — PLAN.md §3 v2（多模态前瞻：assets/resource/tag + job_queue/settings/provider_registry）
-- 迁移纪律：只加不改不删。

CREATE TABLE assets (
    assetId         INTEGER PRIMARY KEY,
    mediaType       TEXT    NOT NULL DEFAULT 'image' CHECK (mediaType IN ('image','video','audio','pdf','other')),
    absPath         TEXT    NOT NULL,
    storeMode       TEXT    NOT NULL DEFAULT 'reference' CHECK (storeMode IN ('reference','copyin')),
    missing         INTEGER NOT NULL DEFAULT 0 CHECK (missing IN (0,1)),
    fileName        TEXT    NOT NULL,
    fileExt         TEXT    NOT NULL,
    fileSz          INTEGER NOT NULL,
    mtime           INTEGER NOT NULL,
    fileHash        TEXT,                          -- SHA-256，物理去重锚点
    title_ch        TEXT,
    title_en        TEXT,
    description     TEXT,
    source          TEXT    NOT NULL DEFAULT 'userUpload',
    parentAssetId   INTEGER REFERENCES assets(assetId),
    searchable      INTEGER NOT NULL DEFAULT 1 CHECK (searchable IN (0,1)),
    deleted         INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0,1)),
    approvalStatus  TEXT    NOT NULL DEFAULT 'approved',
    exif            TEXT,                          -- json：EXIF/视频流信息/音频标签，按 mediaType 结构不同
    importedAt      INTEGER NOT NULL,
    starRating      INTEGER NOT NULL DEFAULT 0 CHECK (starRating BETWEEN 0 AND 5),
    colorLabel      TEXT,
    cloudAssetId    TEXT UNIQUE,                   -- 云端不透明引用，类型前缀 image:<SN> / video:<SN>；NULL=未同步
    cloudSyncedAt   INTEGER
);
CREATE INDEX idx_assets_filehash ON assets(fileHash);
CREATE INDEX idx_assets_mediatype ON assets(mediaType);
CREATE INDEX idx_assets_parent ON assets(parentAssetId);
CREATE INDEX idx_assets_deleted ON assets(deleted);
CREATE INDEX idx_assets_cloud ON assets(cloudAssetId);

-- 检索最小单元（挂向量的粒度）：v1 图片 1 asset = 1 resource(primary)；
-- 视频多关键帧 / PDF 多页 = 1 asset N resource（frame/page），region 为手动裁区。
CREATE TABLE resource (
    resourceId  INTEGER PRIMARY KEY,
    assetId     INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    role        TEXT    NOT NULL CHECK (role IN ('primary','frame','page','region')),
    ordinal     INTEGER NOT NULL DEFAULT 0,
    thumbPath   TEXT,
    width       INTEGER,
    height      INTEGER,
    durationMs  INTEGER,
    pHash       TEXT,
    rect        TEXT                            -- json：region 归一化框 [x,y,w,h]
);
CREATE INDEX idx_resource_asset ON resource(assetId);
CREATE INDEX idx_resource_phash ON resource(pHash);

-- 向量表 vectors_<model> 由 D5 按模型动态建（resourceId 粒度，禁跨模型混算），此处不留静态表。

-- tag 独立表（从 assets.tags json 拆出，可索引可批量）
CREATE TABLE tag (
    id        INTEGER PRIMARY KEY,
    assetId   INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    name      TEXT    NOT NULL,
    origin    TEXT    NOT NULL DEFAULT 'auto' CHECK (origin IN ('auto','manual')),
    model     TEXT,
    score     REAL    NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL
);
CREATE UNIQUE INDEX uq_tag_asset_name ON tag(assetId, name);
CREATE INDEX idx_tag_name ON tag(name);

-- 任务队列（target 为 resourceId 粒度：视频逐帧建 job）
CREATE TABLE job_queue (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    targetId    INTEGER NOT NULL,
    payload     TEXT,                           -- json
    status      TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','done','failed')),
    attempts    INTEGER NOT NULL DEFAULT 0,
    lastError   TEXT,
    createdAt   INTEGER NOT NULL,
    updatedAt   INTEGER NOT NULL
);
CREATE INDEX idx_jobq_status ON job_queue(status);
CREATE INDEX idx_jobq_target ON job_queue(targetId);

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,                   -- json
    updatedAt  INTEGER NOT NULL
);

CREATE TABLE provider_registry (
    name       TEXT PRIMARY KEY,                -- ModelSpec.name，对齐 registry/models.yaml
    spec       TEXT NOT NULL,                   -- json：ModelSpec
    enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    updatedAt  INTEGER NOT NULL
);
