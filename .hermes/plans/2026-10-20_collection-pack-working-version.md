# 图片集 + 行业包 + 单工作版本（合规处理产物与导出闭环）

> 来源：2026-10-20 与用户的连续裁决（damdesk / soujpg-dam-desktop）。
> 状态：设计已定，待建卡实施。本文是唯一口径，后续实现与验收以此为基线。
> 仓库基线：`main @ 20a16ff`（gpu7 `/data/projects/soujpg-dam-desktop`）。

## 0 一句话

**集是身份，包是口径，成品是产物。** 一个图片集 = 一次导入的根目录 = 一个一等实体；一个集只绑一个行业包；一次审核产出一张工作版本；原图永不修改。检索只认原图，展示默认成品。

---

## 1 现状实测（问题与证据）

| # | 事实 | 代码位置 | 后果 |
|---|---|---|---|
| 1 | judge 的 fixer 产物只写 `outDir`，不入库；`{data_dir}/compliance/fixes/{assetId}_{ruleId}_{plugin}.{ext}` 扁平命名 | `app-web/src/compliance_api.rs:889,953`、`svc-pipeline/src/compliance_queue.rs:2569` | 用户看不到、拿不到、不可导出；同名互相覆盖 |
| 2 | 无清理、无列表/下载 API（routes 见 `compliance_api.rs:1845-1867`） | — | fixes 只增不减 |
| 3 | 每条规则各从原图修一次（`let src = PathBuf::from(&ctx.snap.abs_path)`） | `compliance_queue.rs:2567`、`2025` | A3 换底 + A2 超分都 pass，却没有一张同时满足两者的文件 |
| 4 | do 链是链式（上一步产物=下一步输入） | `compliance_queue.rs:2116,2153` | 唯一可复用的叠加形态 |
| 5 | 组级判定明确不落产物 | `compliance_queue.rs:1817` | 组级规则不能修，只出结论 |
| 6 | 导出内容只有 `original` / `thumb256` / `thumb1024`，源为 `assets.absPath` 或缩略图缓存 | `svc-library/src/export.rs:72-77`、`app-web/src/export_api.rs:87-93` | 报告写"合格（已修复）"，导出的是未修复原图 —— 结论与交付物不一致 |
| 7 | 判定输入恒为原图，判定行按 `(packId, assetId, ruleId)` 取最新一行；重跑只删本 job 的行 | `compliance_sqlite.rs:55`、`find_verdicts` | 多轮重复修同一张，链不累积；多包结论混读 |
| 8 | 包激活是全局单值 `settings.ACTIVE_PACK_KEY`，换包只换"界面看哪个 packId" | `compliance_api.rs:650-678` | 无法表达"一个图集一个行业属性" |
| 9 | 无"图片集"实体：仅有 `import_batches(rootPath)`、`/api/library/dirs` 全表扫 absPath、`dirPrefix` LIKE 过滤 | `0002_import_batches.sql`、`library_api.rs:484`、`svc-library/lib.rs:158` | 图集=路径前缀约定，无名字、无归属、`/x/a` 会串到 `/x/a-old` |
| 10 | copyin 落盘 `{root}/{hash[:16]}/{原名}`，原始层级丢失；reference 保留原路径 | `svc-import/src/lib.rs:258-270` | 目录层级只在 reference 下有意义 |
| 11 | 缩略图恒存内部：`{data}/thumbs/{256|1024}/{assetId%100:02}/{assetId}_{size}.webp`，与源层次无关 | `core-storage/lib.rs:48`、`svc-import/lib.rs:480` | 已是期望形态，无需改 |
| 12 | `assets.parentAssetId` 与索引已存在但全库无写入方（仅透传到前端） | `0001_init.sql:17,27`、`svc-library/lib.rs:247,286` | 语义空闲 |

---

## 2 裁决（7 条）

### 裁决 1 — 图片集是一等实体，路径不是组织维度
- 一个图片集 = 一次导入的根目录（`rootPath` 唯一）。名称默认取 rootPath 末段，用户可改。
- **全部扁平展示**：侧栏 = 图片集列表（名称 + 张数 + 行业包徽标），点开平铺网格。**不保留任何子目录层级**，不做"可选展开"，不留目录树。
- 路径只用于两件事：定位原图、详情页可点开所在文件夹（KI-10 PathLink）。
- 同 rootPath 重复导入 → 归到同一集，不新建。集删空后保留（命名/绑包是有价值的状态）。

### 裁决 2 — 一个集合只绑一个行业包
- `collection.packId` 单值。想跑两个行业口径 = 拆成两个集（改名/分目录），不是让资产记多个包。
- pack 由"资产所属集合的绑定"决定，不接受"全局激活包 + 用户手选"。

### 裁决 3 — 单工作版本（working copy），不做版本栈
- root 原图永不修改、永不删除；派生成品最多 1 个：`{data}/derived/{collectionId}/{assetId}.{ext}`。
- 新审核轮开始 = 删旧成品（文件 + 派生资产行 + 其判定行），从原图按新包重新判/修。
- 明确不做：版本链 v1..vN、Frame.io 版本面板、设回主版本、分屏对比、stale 二维结论、CAS 并发。
- 审计只留痕不保像素：删成品前落一行 `collection_ops`。

### 裁决 4 — 一轮审核 = 一张成品
- 一轮 job 内 do 链 + judge 的 fixer **串行叠加**，产出一张成品；不是每规则一张。
- 中间步骤文件可落盘作溯源，但不建版本行、不进导出。
- 修不了（模型缺失/复检不过）→ 该规则 fail/undetermined 转人工，**不产半成品**。

### 裁决 5 — 派生图不入索引
- 派生图不参与 embedding、判重（`dup_edges`）、自动打标、OCR、EXIF 解析、统计计数、搜索。
- 检索单元 = 原图（root）；展示单元 = 成品（存在则展示成品，附「已处理」徽标 + 原图/成品切换）。
- 派生图只需生成缩略图（展示用）。

### 裁决 6 — 换包四步（闭环，可审计）
1. 绑定变更：`collection.packId: A → B`，`collectionId` 不变（不搬家、不重建资产、不动标签）。
2. 互斥：该集若有审核 job 在跑，先取消或等收工（集级锁），否则统计作废。
3. 作废旧成品：删该集成品文件，集内资产标记待重跑；`collection_ops` 记一行（A→B、operator、时间）。
4. 重跑：输入恒为原图 + 包 B。旧包判定行**不删**（按 packId 天然隔离），界面/导出只认当前包 + 当前成品。

### 裁决 7 — 追踪三层，不混
| 层 | 载体 | 内容 |
|---|---|---|
| 集合事件流 | 新表 `collection_ops`（照 `migrate_audit` 形态） | 导入 / 绑包 / 换包 / 跑审核 / 导出 / 改名 / 删集；operator、时间、结果 |
| 轮次结果 | `compliance_runs` 加 `collectionId`（params 已有 packId/scope） | 本轮包、通过/修复/失败、耗时 |
| 资产处理状态 | 成品 + `derivedBy`（packId/jobId/rules/plugins） | 当前成品由哪轮哪个插件产出 |

边界：追踪 ≠ 保留像素；写入点在服务端（前端只读）；不新造"集版本"概念。

---

## 3 数据模型（migration 0025）

```sql
CREATE TABLE collection (
    collectionId INTEGER PRIMARY KEY,
    rootPath     TEXT NOT NULL UNIQUE,   -- 去尾分隔符归一化
    name         TEXT NOT NULL,          -- 默认 rootPath 末段，用户可改
    packId       TEXT,                   -- 当前行业包（NULL=未绑）
    createdAt    INTEGER NOT NULL,
    updatedAt    INTEGER NOT NULL
);

ALTER TABLE assets   ADD COLUMN collectionId INTEGER REFERENCES collection(collectionId);
ALTER TABLE assets   ADD COLUMN derivedBy    TEXT;     -- json: {packId,jobId,rules[],plugins[],ts}；NULL=原图
ALTER TABLE assets   ADD COLUMN derivedOf    INTEGER;  -- 成品行 → 原图 assetId；原图为 NULL
CREATE INDEX idx_assets_collection ON assets(collectionId);
CREATE INDEX idx_assets_derived_of ON assets(derivedOf);

CREATE TABLE collection_ops (
    id           INTEGER PRIMARY KEY,
    collectionId INTEGER NOT NULL REFERENCES collection(collectionId) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN
                   ('import','bind_pack','switch_pack','run_compliance','export','rename','delete_fixed')),
    detail       TEXT,               -- json：packId 前后值、runId、导出目标、删除的成品数…
    operator     TEXT NOT NULL DEFAULT 'local',
    status       TEXT NOT NULL CHECK (status IN ('done','failed','cancelled')),
    error        TEXT,
    at           INTEGER NOT NULL
);
CREATE INDEX idx_collection_ops_coll ON collection_ops(collectionId, at);

ALTER TABLE compliance_runs ADD COLUMN collectionId INTEGER;
```

回填（迁移内一次完成，幂等）：
- 对每个 `import_batches.rootPath` 生成/复用 collection（按 `trim_dir_sep(rootPath)` 去重）；
- 资产回填：`absPath` 落在该 rootPath 子树下者取**最长匹配**得 collectionId；无匹配者归到 rootPath 为 `<库根>` 的兜底集（迁移时建一条，`name='未归类'`）；
- `smart_folders` 存量 query 清掉 `dirPrefix` 键（一次性，不兼容旧口径）。

字段语义钉死：
- `assets.derivedOf` / `derivedBy` 只标"成品 vs 原图"，**不建版本链**（不用 `parentAssetId` 冒充版本；`parentAssetId` 保持空闲，留给将来"来源溯源"）。
- `assets.absPath` 仍是唯一物理定位（成品路径直接写在成品行的 absPath）。

---

## 4 流程

### 4.1 导入
1. 用户选 root 目录 → `import_batches` 一行（沿用）。
2. 按 `trim_dir_sep(rootPath)` 找/建 `collection`；`insert_asset` 写 `collectionId`。
3. 缩略图沿用 `{data}/thumbs/...`（不变）。
4. `collection_ops` 记 `import`（张数、storeMode）。

### 4.2 跑审核（一轮）
1. 入口：在某个集（或选中集内资产）上触发，pack 取自 `collection.packId`（未绑 → 报错要求先绑，不猜）。
2. 集级互斥：同集已有 running 的 compliance run → 拒绝并提示（不排队，语义清楚）。
3. 开工前删该集旧成品（文件 + 派生资产行 + 派生行的判定行），记 `delete_fixed`。
4. 逐资产：判定输入 = 原图 → 规则按 pipeline 顺序执行 → do 链产物与 judge fixer 串行叠加 → **唯一链尾**写 `{derived}/{collectionId}/{assetId}.{ext}` → 插派生资产行（`derivedOf`/`derivedBy`/`collectionId`）+ 生成缩略图 + 写判定行（判定行挂**原图 assetId**，成品不单独判）。
5. 收工：`compliance_runs.collectionId` + `collection_ops` 记 `run_compliance`（包、张数、结论分布）。

### 4.3 换包
见裁决 6 四步；`collection_ops` 分别记 `switch_pack` / `delete_fixed` / `run_compliance`。

### 4.4 导出
- `ExportContent` 增加 `Fixed`（成品）；UI 给三选一：`原图` / `处理结果` / `缩略图 256/1024`。
- 默认按"有成品就用成品"，无成品的资产在报告里单列"未处理"（不静默回退成原图且不说明）。
- manifest/判定报告增加三列：`处理状态`（未处理/已处理）、`处理包`、`处理插件`；报告头写明本次导出内容口径。
- 成品缺席（文件丢失）→ 计入 `ExportSkipped`，与现有的 missing 语义一致。

### 4.5 展示
- 侧栏：集列表（名称、张数、包徽标、最近审核时间）；点开平铺网格，**无目录层级**。
- 卡片：有成品 → 展示成品缩略图 + 「已处理」徽标；详情面提供「原图 / 处理结果」切换（不叫版本）。
- 删除单个成品（退回原图）= 删成品行 + 文件 + 记 `delete_fixed`。

---

## 5 落地卡

### 卡 A — 数据与后端（无前端改动）
1. migration 0025 + 回填 + `smart_folders` 清洗（幂等脚本，跑两遍结果一致）。
2. 导入写 `collectionId`（`svc-import`），同 rootPath 复用集合。
3. `GET /api/collections`（列表 + 张数 + packId + 最近 run）；`PATCH /api/collections/{id}`（改名、绑包）。
4. `filter_where`：`dirPrefix LIKE` → `collectionId = ?`；`/api/library/dirs` 下线；`dirPrefix` 字段全链路删除（core-types / search_api / export_api / SmartFolder query）。
5. 导入无 pack 绑定拒绝审核；集级互斥；开工删旧成品；一轮一成品落 `derived/{collectionId}/`；`derivedBy` 溯源；成品不入 embed/判重/打标/OCR/统计。
6. `ExportContent::Fixed` + 报告三列。
7. `collection_ops` 五个写入点（import / bind_pack / switch_pack / run_compliance / delete_fixed / export）。

### 卡 B — 前端（依赖 A）
1. `LibraryTree` → 集列表（名称 + 张数 + 包徽标 + 最近审核），点开平铺网格；`buildTree`/`isUnderRoot`/`pathSegs` 整块删除。
2. `Gallery` / `galleryUrlState` / `persistedGallery` / `urlFilter`：`dirPrefix` → `collectionId`。
3. Search / ExportPage 的目录前缀输入 → 集选择。
4. 卡片「已处理」徽标 + 详情「原图 / 处理结果」切换。
5. 导出对话框加内容三选一 + 未处理单列提示。
6. 相关测试同步（`pathRevealTouchpoints.dom`、`galleryUrlState`、`persistedGallery`、`searchQuery`、`SmartFolderDialog`）。

---

## 6 验收口径

1. 导入一个 3 层目录 → 侧栏只出现 1 个集，张数正确，网格平铺，无任何子目录节点。
2. 绑包 → 跑审核 → 导出 `处理结果`：导出文件的四角/尺寸满足包规则（对比原图确实不同）；导出 `原图` 与原文件字节级一致（`sha256` 相同）。
3. 同一资产跑第二轮（换包）：旧成品消失、新成品生成、旧包判定行仍可按 packId 查回、`collection_ops` 有完整 A→B→run 轨迹。
4. 成品文件 `collectionId` 归属正确；成品不出现在搜索命中、不计入集张数、不进 dup_edges、无 embed/OCR/tag 行。
5. 换包时该集有 running 审核 → 明确拒绝并给出原因（不静默覆盖）。
6. 集内某规则修不了（模型缺失）→ 不产半成品、规则标 undetermined、集事件流有记录。
7. 删除成品 → 卡片回原图、文件消失、ops 留痕；原图字节不变（`sha256` 前后一致）。

---

## 7 明确不做（防止再次长复杂度）

- 版本栈 / 版本链 / v2 徽标 / 版本面板 / 设回主版本 / 分屏对比
- 多包叠加累积（A 包修完 B 包继续在同一链上加一层）
- 二维结论（合格 = f(pack, version)）与 stale 判定口径
- copyin 保留相对路径迁移（集合归属写进 `collectionId` 后不再需要）
- 目录树 / 子目录分组 / 可展开层次 / `dirPrefix` 过滤
- 集级审批工作流（`approvalStatus` 维持资产级现状，本次不动）

## 8 风险与已知代价

| 风险 | 说明 | 处置 |
|---|---|---|
| 回不到上一轮成品 | 换包即删成品 | 回原图重跑；同包同参数结果可复现，跨版本模型不保证 |
| 交替跑两个包互相覆盖 | 语义即"谁最后跑谁算数" | UI 换包时明确提示"将替换已有处理结果" |
| 一次性删 `dirPrefix` | 19 个文件涉及（含测试）、smart_folders 存量 query | 迁移内清键 + 全量 L0 回归 |
| 双图（原图/成品）计数易错 | 统计、张数、导出计数三处必须同源 | 统一 current 视图：`derivedOf IS NULL` 才是资产行 |
