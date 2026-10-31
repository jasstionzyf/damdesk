-- 0034 ocr_boxes — DM-C1（task）：OCR 文本行框落库（inpaint 重绘区域的唯一可复核来源）。
--
-- 为什么必须落库（不是运行期现算）：
-- * VL 返回的 bbox 现在直接当重绘区域用 —— 没有任何独立证据能证明那块地方「真有东西」。
--   OCR 是本地确定性通道（PP-OCRv6 det/rec），它的行框就是「这里有文本」的**实测证据**；
--   不落库的话，判定那一刻无从复核（引擎-ocr 的行框目前只活在 `ocr_one` 的栈上）。
-- * 重绘区域与复检区域必须在报告里对得上（K3 报告 §6）：区域来自哪一路要能事后审计
--   （evidence.region.source），框的坐标就是审计对象。
--
-- 坐标口径：**归一化 0..1**（源图像素 / 宽高）。理由：消费方（VL regions、inpaint box）
-- 全用相对坐标，存像素就要在每次消费时重新取宽高（宽高缺失的历史资产会直接读不出框）；
-- 归一化把「图多大」这个变量在写入口就消化掉。
--
-- 迁移纪律（PLAN.md §3）：只加不改不删。
CREATE TABLE IF NOT EXISTS asset_ocr_boxes (
    assetId     INTEGER NOT NULL REFERENCES assets(assetId) ON DELETE CASCADE,
    -- 阅读序下表内序号（0 起；同一资产重跑 OCR = 整组替换，序号不跨版本复用）
    idx         INTEGER NOT NULL,
    x0          REAL    NOT NULL,
    y0          REAL    NOT NULL,
    x1          REAL    NOT NULL,
    y1          REAL    NOT NULL,
    -- 行文本（rec 输出，可能为空串 = 检出框但无字符）
    text        TEXT    NOT NULL DEFAULT '',
    -- DB det 框得分（bbox 区域均值）与 CTC 字符置信度均值（0..1）
    boxScore    REAL    NOT NULL DEFAULT 0,
    textScore   REAL    NOT NULL DEFAULT 0,
    PRIMARY KEY (assetId, idx)
);

-- 消费口径：按资产取全部行框（重绘区域决议 + 后续 OCR 框复核）。资产删除靠外键级联。
CREATE INDEX IF NOT EXISTS idx_asset_ocr_boxes_asset ON asset_ocr_boxes(assetId);
