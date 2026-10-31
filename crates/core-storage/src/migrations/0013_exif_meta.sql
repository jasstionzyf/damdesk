-- 0013 exif_meta — EXIF2（task）：拍摄元数据派生列 + 画幅比派生列。
-- 迁移纪律：只加不改不删。exif(json) 列 0001 已有（真值摘要），这里加**可检索派生列**。
-- takenAt = unix 秒（DateTimeOriginal − OffsetTimeOriginal；缺偏移按 UTC；无拍摄时间
-- 保持 NULL，绝不拿 mtime 顶替）。方向口径：5..=8 需宽高互换。
-- aspectClass 判据（与代码 aspect_class 一致）：|ratio−1| ≤ 0.02 → square（±2% 方图容差）；
-- ratio = w/h；w/h 缺失（解码失败）→ 两列都 NULL。

ALTER TABLE assets ADD COLUMN takenAt INTEGER;
ALTER TABLE assets ADD COLUMN cameraMake TEXT;
ALTER TABLE assets ADD COLUMN cameraModel TEXT;
ALTER TABLE assets ADD COLUMN lensModel TEXT;
ALTER TABLE assets ADD COLUMN iso INTEGER;
ALTER TABLE assets ADD COLUMN focalLenMm REAL;
ALTER TABLE assets ADD COLUMN aperture REAL;
ALTER TABLE assets ADD COLUMN shutterText TEXT;
ALTER TABLE assets ADD COLUMN orientation INTEGER;
ALTER TABLE assets ADD COLUMN aspectRatio REAL;
ALTER TABLE assets ADD COLUMN aspectClass TEXT;
ALTER TABLE assets ADD COLUMN gps_lat REAL;
ALTER TABLE assets ADD COLUMN gps_lon REAL;
ALTER TABLE assets ADD COLUMN gpsSource TEXT;

CREATE INDEX idx_assets_takenat ON assets(takenAt);
CREATE INDEX idx_assets_cameramodel ON assets(cameraModel);
CREATE INDEX idx_assets_aspect ON assets(aspectClass);
