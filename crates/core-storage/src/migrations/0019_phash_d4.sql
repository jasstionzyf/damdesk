-- KI-2c（task）：pHash 升级为 D4 八变体（4 旋转 × 是否镜像），旋转/镜像图不再固定漏判。
--
-- 存储裁决：新增 `pHashes` TEXT 存 JSON 数组
--   * 每项 16 位十六进制小写字符串，下标顺序 = engine-dedup `D4_ALL`
--     （0=identity 1=旋转90° 2=旋转180° 3=旋转270° 4=水平镜像 5=主对角镜像 6=垂直镜像 7=副对角镜像）
--   * 老列 `resource.pHash` **原位保留 = identity 变体**：旧包/旧读路径照常工作，
--     多值列缺失时读侧回落到单值（`engine_dedup::phashes_for_compare`），不存在双写/双读合并逻辑
--   * 为什么不是 8 个定长列：变体数将来要扩（如加 16 变体）只需改值，不再迁表结构
--
-- 回填口径（老库 = 单一 pHash → 视为 identity 变体）：
--   * 只补 `pHashes IS NULL` 的行 → 天然幂等，重跑不改任何已有行
--   * `pHash` 为空/NULL 的行不编造值（保持 NULL = 「无 pHash」，判重侧照旧不参与近似边）
ALTER TABLE resource ADD COLUMN pHashes TEXT;

UPDATE resource
   SET pHashes = '["' || lower(trim(pHash)) || '"]'
 WHERE pHashes IS NULL
   AND pHash IS NOT NULL
   AND trim(pHash) <> '';
