# SOUJPG DAM Desktop — 实施计划（v1）

> 开源桌面版 DAM。裁决链：2026-10-12 桌面版=开源 DAM 桌面发行版 → PLG 漏斗（隐私本地+协作/多端断点）→ 本仓 `soujpg-dam-desktop`。
> 参考实现：souJpg 云 DAM（image-front-api `vcgImageAI/subProjects/dam/damService.py`、images_meta 模型、VL 管线）+ dam-ai 三引擎（OpenAI 兼容 API）。
> 产品需求蓝本：soujpg-product-dam skill（见 ~/.hermes/profiles/soujpg/skills/soujpg/product/soujpg-product-dam/）。

## 0. 原则（先读）

1. **字段对齐 images_meta**：本地 SQLite 字段名与 images_meta 同名同义（title_ch/tags/source/parentAssetId/approvalStatus/searchable/deleted...），本地新增字段只加不改。
2. **业务中性**：开源仓不出现 souJpg 商业概念（计费/点数/审批中的商业化逻辑/DP 采购）。souJpg 品牌仅出现在仓库名与 README 徽章。
3. **推理可插拔**：一切推理走 OpenAI 兼容 endpoint（本地 sidecar dam-ai 或用户自配公有云 VL API），desktop 不绑定厂商。
4. **GPU 可选**：无 GPU/CPU 慢/完全离线均可降级使用，导入永不阻塞。
5. **License AGPL-3.0**（与开源策略一致）。
6. **分层架构铁律（2026-10-12 裁决）**：模块只允许向下引用、禁止同层互引与向上引用；依赖方向单向；见 §2.1（cargo 工作区分层 enforced）。

# 二、架构与模块设计

## 2.1 分层与模块划分（核心裁决：向下引用、单向依赖、无同层互引）

### L0 — foundation（无内部依赖）

| 模块 | 职责 | 对外不依赖 |
|------|------|------------|
| `core-types` | Asset/Resource/Tag/JobSpec 等全部领域结构体 + 错误类型（thiserror） | ✗ |
| `core-storage` | SQLite 封装（连接池/迁移/DAO）、向量库（vecstore：sqlite-vec 按 embModel 分表，D5 裁决）、文件与目录工具 | ✅ |
| `core-hash` | SHA-256 / pHash（含 8x8 DCT）| ✗ |
| `core-media` | libvips 封装：解码/缩略图/主色/波形占位 | ✗ |
| `core-config` | settings/provider 配置读写、models.yaml 解析（ModelSpec） | ✗ |

### L1 — engine（只依赖 L0）

| 模块 | 职责 |
|------|------|
| `engine-embed` | EmbedProvider trait + OnnxSiglipProvider + OpenAiCompatProvider |
| `engine-tag` | TagProvider trait + OpenAiCompatTagProvider |
| `engine-classic` | 主色/调色板/均值 hash 等纯算法 |
| `engine-jobq` | job_queue 调度器（状态机、重试、断点续跑；不关心 job 语义） |
| `engine-search` | 检索编排：向量召回 + SQLite 标量过滤 + 合并排序（不含 UI 语义） |
| `engine-dedup` | fileHash/pHash/向量聚类分组 |

### L2 — service（编排层，依赖 L0+L1，可互不引用；有交叉需求向上提 L1/L0）

| 模块 | 职责 |
|------|------|
| `svc-import` | 扫描/watcher/去重入库/缩略图/批次断点（编 L0 media+hash+storage，L1 无） |
| `svc-pipeline` | 打标/embedding 编排（编 L1 jobq+embed+tag，asset/resource 状态机） |
| `svc-sync` | 云同步预留（cloudAssetId 状态机；v1 空实现） |
| `svc-library` | 面向 UI 的读模型：列表/过滤/统计/标签面板（纯读） |

### L3 — app（进程壳，唯一组装点）

| 模块 | 职责 |
|------|------|
| `app-tauri` | Tauri 命令层（薄壳：参数校验→调 L2 →返回/事件）；**业务逻辑零容忍**，全部下沉。W7+ 唯一原生能力 = 系统文件夹选择框（`dialog:allow-open`），其余全走内嵌 app-web HTTP |
| `src/`（React） | 纯 UI：views/components/stores/lib(ipc)；只通过 invoke 与 app-tauri 说话，不知 Rust 模块存在 |

### 依赖规则（CI enforced）

- 方向：L3 → L2 → L1 → L0，同层禁止互相 use（cargo-workspace 单 crate 即天然编译隔离；跨层只经 pub trait/结构体）
- `core-types` 是唯一全共享类型层（避免 storage/engine 各造轮子）；错误链 thiserror `#[from]` 逐层向上转译，L3 统一映射 invoke 错误码
- React 侧同理：components 不 import views；stores 只被 views 用；lib/ipc 是唯一 IPC 出口
- **新功能落位法**：先问「是不是新推理能力」→ L1 加 provider；「是不是新编排」→ L2 加 service；「是不是新存储/算法」→ L0/L1 对应模块。禁止在 app-tauri 直接写业务（代码评审红线）

## 1. 仓库与形态

- Gitea: `http://172.25.45.244:13000/zhaoyufei/soujpg-dam-desktop`（已建，main 分支 auto-init）
- 形态：Tauri 2 桌面 App（Rust core + React UI）
- Monorepo 布局：

```
soujpg-dam-desktop/            # cargo workspace（分层 = crate 边界，编译器强制）
├── crates/
│   ├── core-types/            # L0 领域类型+错误
│   ├── core-storage/          # L0 SQLite/向量库/文件
│   ├── core-hash/             # L0 哈希
│   ├── core-media/            # L0 libvips
│   ├── core-config/           # L0 配置/registry
│   ├── engine-embed/          # L1 EmbedProvider×2
│   ├── engine-tag/            # L1 TagProvider
│   ├── engine-classic/        # L1 经典算法
│   ├── engine-jobq/           # L1 任务调度
│   ├── engine-search/         # L1 检索
│   ├── engine-dedup/          # L1 去重聚类
│   ├── svc-import/            # L2 导入编排
│   ├── svc-pipeline/          # L2 打标/嵌入编排
│   ├── svc-sync/              # L2 云同步（预留）
│   ├── svc-library/           # L2 读模型
│   └── app-tauri/             # L3 进程壳（src-tauri 入口，引用上四层）
│       └── …前端 dist 打进二进制
├── src/                       # React UI（views/components/stores/lib/ipc）
├── tests/                     # 跨 crate 集成测试 + mock server + fixtures
├── docs/                      # architecture.md（本节展开版）
├── scripts/                   # verify_all.sh 等
├── registry/models.yaml       # provider 声明式注册（core-config 解析）
├── dev/                       # 开发容器
├── reports/                   # kanban 证据
├── LICENSE                    # AGPL-3.0
└── README.md
```

## 2. 总体架构

```
┌─ Tauri 2 单进程 ─────────────────────────────────────────────┐
│ React TS：纯 UI（网格/搜索/设置/进度）                         │
│      │ invoke                                                │
│ Rust core（一切推理在进程内）：                                 │
│   importer   扫描/拖拽 → libvips 缩略图 → hash 去重 → watcher  │
│   indexer    SQLite(assets/resource/tag) + 向量库(按model分表) │
│   pipeline   job_queue：embed/tag 任务（断点续跑/重试）        │
│   embedder   EmbedProvider trait：                             │
│     ├ OnnxSiglipProvider（默认：ort+SigLIP2 内嵌，零配置）      │
│     └ OpenAiCompatProvider（/v1/embeddings 云多模态 embedding）│
│   tagger     TagProvider trait → OpenAI 兼容云 VL API（可选）   │
│   classic    主色/调色板 纯 Rust                               │
│   search     文搜图/以图搜图/复合过滤（向量挂 resource 粒度）   │
│   dedup      pHash/fileHash + 向量聚类                         │
└───────────────────────────────────────────────────────────────┘
```

## 3. 数据模型（v2 重设计 2026-10-12：多模态前瞻——图片/视频/音频/PDF 一套表）

设计原则：
1. **媒体无关的 asset 主干**：凡是「一个文件产生一个资产」的格式（图/视频/音/PDF）共用 assets 表；格式特有属性一律拆到 media_type 子表，不往主干塞列
2. **资源锚点（resource）与资产（asset）分离**：同一文件（fileHash 相同）被导入多个库/多次引用只存一份物理记录；视频的多关键帧、PDF 的多页 = 一个 asset 挂多个 resource（帧/页是可被检索/可视化的最小单元）
3. **向量挂 resource 不挂 asset**：一视频抽 5 帧各一向量、一 PDF 每页一向量，搜到帧/页后回溯 asset——这是支持视频/PDF 语义搜索的关键
4. **provider 产物全部带溯源**：embModel/tagModel 字段化，向量表按 model 分表（禁混算），tags 带 origin+model
5. 迁移纪律：SQLite migrations 只加不改不删；字段对齐 images_meta 的映射关系保留在 §3.2

### assets（主干，媒体无关）

| 字段 | 类型 | 说明 |
|------|------|------|
| assetId | INTEGER PK | 本地主键（不再叫 imageSN——未来非图片） |
| mediaType | TEXT | image / video / audio / pdf / other（查询路由用，v1 只写 image） |
| absPath | TEXT | 文件绝对路径（同 hash 多引用时多条 asset 可指向同 path 或不同 path） |
| storeMode | TEXT | reference / copyin |
| missing | INTEGER | 0 正常 1 失联（可 relink） |
| fileName / fileExt / fileSz / mtime | | 文件四要素；fileExt 白名单决定 mediaType 路由 |
| fileHash | TEXT | SHA-256（物理去重锚点；索引 idx_filehash） |
| title_ch / title_en / description | | AI 或手动生成 |
| source | TEXT | 本地固定 userUpload（对齐云端语义） |
| parentAssetId | INTEGER | 血缘：视频抽帧图/衍生图指向源 |
| searchable / deleted / approvalStatus | INTEGER/TEXT | 对齐 images_meta 同名同义 |
| exif | TEXT(json) | 通用元数据摘要（EXIF/视频流信息/音频标签，按 mediaType 结构不同） |
| importedAt | INTEGER | |
| starRating | INTEGER | 用户收藏 0-5（本地新增，图片产品标配） |
| colorLabel | TEXT | 用户色标（红/黄/绿…，Lightroom 习惯） |
| cloudAssetId | TEXT | 云端资产引用（同步预留，不透明值带类型前缀：`image:228123456` / 未来 `video:...`）。**单字段泛化**，不按媒体类型加 cloudImageSN/cloudVideoSN——云端分表是其内部命名空间，desktop 只存引用（UNIQUE，NULL=未同步） |
| cloudSyncedAt | INTEGER | 最近成功同步时间（增量同步判据） |

### resource（检索最小单元；v1 图片=1 asset 1 resource）

| 字段 | 类型 | 说明 |
|------|------|------|
| resourceId | INTEGER PK | |
| assetId | INTEGER | FK → assets；idx_assetid |
| role | TEXT | primary（代表帧/首图）/ frame / page / region |
| ordinal | INTEGER | 帧/页序号 |
| thumbPath | TEXT | 该 resource 的缩略图 |
| width / height | INTEGER | 像素尺寸（region=归一化坐标另存 rect json） |
| durationMs | INTEGER | 音视频 resource 的时长（帧的时间戳） |
| pHash | TEXT | 感知哈希（图片帧/页快照） |
| rect | TEXT(json) | region 类型时的归一化框 [x,y,w,h] |

### 向量（**sqlite-vec 裁决 2026-10-13**，按 embModel 分表 `v_<model>`，vec0 虚表在 dam.db 内）

> D5 spike 实测否决 LanceDB（需 protoc、arrow 版本地雷、datafusion 拖包），选 sqlite-vec 0.1.9：
> 100K×512 KNN 40ms（验收 <1s 的 24 倍余量）、4.3s 冷编、零额外打包依赖、向量与标量同库。
> 详见 `reports/D5-spike-decision.md`。≥百万级库时重开 spike。

| 字段 | 类型 | 说明 |
|------|------|------|
| resourceId | INTEGER PK | **挂 resource**：视频帧/PDF 页各一向量，命中回溯 asset |
| embedding | float[dims] | cosine（distance=1-cos）；归一化 |
| （表登记） | vector_tables | model/dims/tableName 登记簿（migration 0005） |

铁律：vec0 的 JOIN 是 lossy post-filter——复合过滤走 engine-search 的候选集预过滤
（≤8K 精确打分 / >8K KNN 过采样+回退），不可直接 JOIN。

**双嵌入器（裁决 2026-10-13）**：语义塔 siglip2-base-p224-onnx（768d，图+文，搜索）+
去重塔 dinov3-vits16-onnx（384d，仅图，res 256，查重 find_duplicates 阈值分带
near ≥0.98 / variant [0.90,0.98)）；双表独立建，assets.emb_status 与 dedup_status
两状态机互不阻塞；队列 kind=`embed:<model>`，一键构建先 siglip 后 dino。

### tag（从 assets.tags json 拆为独立表，可索引可批量）

| 字段 | 类型 | 说明 |
|------|------|------|
| id INTEGER PK / assetId FK / name TEXT / origin TEXT(auto/manual) / model TEXT / score REAL / createdAt INTEGER | | idx (assetId,name) 唯一；手动标 score=1.0 origin=manual |

### 其他

- `import_batches`（批次/断点）、`job_queue`（任务队列，target 改为 resourceId 粒度：视频逐帧建 job）、`settings`（provider 配置）、`provider_registry`（models.yaml 镜像，运行时状态）
- **同步映射**（未来）：assetId+resourceId → 云端 images_meta（云端重嵌入，本地向量不上传；对齐字段 title_ch/description/tags/source/parentAssetId/searchable/deleted/approvalStatus 直通）

### 3.2 对齐 images_meta 字段映射（同步契约）

| 云端 images_meta | 本地 v2 | 备注 |
|---|---|---|
| imageSN | assetId（图片时） | 同步时云端分配，本地记 assets.cloudAssetId（泛化单字段，如 image:SN），不随云端扩媒体类型而加字段 |
| title_ch/title_en/description/tags/source/parentAssetId/searchable/deleted/approvalStatus | 同名直通 | |
| embedding_model | vectors_<model>.embModel | 云端重嵌入，本地向量不迁移 |

### 3.3 格式扩展路线（表结构已就绪，v1 只实现 image）

| 格式 | mediaType | resource 生成 | v 计划 |
|------|-----------|--------------|--------|
| 图片 jpeg/png/webp/gif/bmp/avif/heic | image | 1:1 primary | v1 |
| 视频 mp4/mov/mkv/webm | video | 抽 N 关键帧（ffmpeg，1 primary+N frame） | v2 |
| 音频 mp3/wav/flac/m4a | audio | 1:1 primary（缩略=波形图；文本侧靠 ID3 歌词/转写 tag） | v2 |
| PDF | pdf | 逐页渲染（pdfium/mupdf）成 page resource | v2/v3 |
| 文档 docx/xlsx | other→v3 | LibreOffice headless 转 PDF 复用 pdf 管线 | v3 |

**v1 落地约束**：D1 建表即按本节 v2 结构建（assets+resource+tag+cloud 同步字段），但 importer/搜索只走 image 路径——表结构前瞻，功能不超前。

## 4. 模块拆分与排期（M0-M6）

### M0 骨架（1 周）
- Tauri 2 + React(Vite) + TS 项目初始化；Rust 侧 rusqlite + ~~lancedb~~ sqlite-vec 接入（D5 裁决）
- App 外框：左侧库列表/右侧网格的空壳，设置页
- 验收：空库启动/退出无残留进程；`pnpm tauri dev` 可跑

### M1 导入器（2 周）
- 文件夹选择/拖拽导入；并行扫描（ignore 隐藏/系统目录）；格式支持 jpeg/png/webp/gif/bmp/avif
- **存储模式（裁决 2026-10-12）：默认原地索引（reference，不拷贝原图）**，图片留在用户原位置，仅记 absPath；可选「拷贝入库」模式（copy-in 到库目录）。导入向导二选一，默认原地。理由：不动用户数据（隐私叙事一致）、大库免拷贝、不破坏用户现有目录结构
- **desktop 数据目录布局**：`{appData}/soujpg-dam-desktop/`（mac: ~/Library/Application Support，win: %APPDATA%）内含 dam.db（SQLite，**向量 vec0 表同库**）、thumbs/（缩略图，256/1024 两档，按 assetId 分片）。copy-in 模式的库目录默认 `{appData}/library`（Settings「库目录」可配，裁决 2026-10-13 与 appData 分离）
- libvips(rust binding) 生成缩略图（256/1024 两档）；SHA-256 去重（同 hash 跳过）
- 原地索引的失效处理：watcher 检测文件移动/重命名时更新 absPath；删除/失联标记 `missing=1`，UI 有"失联资产"过滤视图，可重新定位（relink）
- 增量：mtime+size 变化重扫；导入批次表记录进度，支持中断续跑
- watcher：notify-rs 监视已导入根目录
- **path 标签（裁决 2026-10-13）：目录名/文件名解析为标签，用户可选开关**。settings 新增 `path_tags: {useDirs: bool, useFilenames: bool}`（裁决 2026-10-13：默认均 false，不开启），导入向导与设置页均可改。规则：①目录取相对导入根的路径段（根目录本身不入标签），每段一个标签并保留父链层级；②文件名去扩展名后丢弃相机/系统噪声（IMG_/DSC_/Screenshot/微信图片_/纯数字段/副本/(1)/- copy 等），剩余按分隔符（空格 _ - （）·）切词，英文归一小写、全半角统一；③中文无分隔符段不内切词，整段作标签；④单个标签 token 超过 6 个字符的丢弃（裁决 2026-10-13：防长句目录名/文件名产废标签，阈值后续可做成配置）；⑤过滤后为空不产标签。标签入 tag 表 origin="path"（与 auto/user 共存，搜索过滤同权，展示可区分来源），入库前规范化去重。导入时一次性解析（零成本）；watcher 检测移动/重命名时仅重解析 origin=path 的标签（删旧增新，不碰 auto/user）；开关关闭时不产生、已有 path 标签保留不影响
- 验收：1 万图导入可中断恢复；重复文件不入库；watcher 检测新增/删除/移动；原地模式下移动文件后资产不丢失（absPath 更新或标 missing）；path 标签开关关闭时导入不产标签、开启后重扫可补齐

### M2 推理接入（1 周）
- 设置页四态配置：sidecar / 自配远程 OpenAI 兼容 / 纯离线
- sidecar 模式：binary 管理（/readyz 探针、拉起/停止）；自配模式：URL+key+model 名（embed 与 tag 可分别配置）
- 打标模板：tagger `/v1/tagging` 通用模板（标题/描述/标签三合一）
- **语义属性生成器选型（裁决 2026-10-13）：不做内置 ONNX 小 VLM**。title_ch/title_en/description/tags 需要生成式 VLM，CPU ONNX 路径不可用（10~30s/图，万图=天级；SigLIP 类只能出 embedding 出不了文本；ORT 拼 VLM 工程量大质量差）。形态：tag client 走 OpenAI 兼容 chat/completions，云 API（dashscope qwen-vl/智谱/OpenAI）与本地 Ollama（有 GPU 的 PC 跑 qwen2.5-vl 等）同一配置口，架构零改动。默认不配 → 语义属性留空，走降级链（本地 SigLIP embedding 搜图 + path 标签 + EXIF），UI 显示「配置打标模型后自动补全」。**tag provider 预设表单（裁决 2026-10-13）**：设置页选 provider（dashscope / zhipu / Ollama / 自定义）→ 动态表单：base_url 按预设预填（可改）+ API key 输入 + 模型下拉（预设内置该家 VL 模型清单，仅多模态模型可作 tag 引擎：dashscope=qwen3-vl-plus，zhipu=glm-4.6v 系，Ollama=扫描本地 /api/tags，自定义=手填）。key 只存本地 settings 不入 git。实测可用组合（2026-10-13）：dashscope qwen3-vl-plus base64 打标正常；zhipu coding 通道 glm-5.3-flash 为文本/reasoning 模型不作 tag 引擎。title_en v1 即双语同出，同步云端不回补
- 验收：四态切换即生效；探针失败引导配置而非崩溃；不配 tag 模型时导入/搜索全流程可用（无标题描述有 embedding）

### M3 自动打标（1 周）
- 打标任务队列（job_queue）：批量分片 64/批，失败重试 3 次，进度条
- 结果写 title_ch/description/tags(origin=auto)；可全库重跑（换模板/换模型）
- **自定义打标任务（裁决 2026-10-13）**：用户可定制打标维度——打标管理界面新建「自定义任务」：标签 name + 提示词 prompt（+输出格式提示），可建多个；执行机制与内置打标相同（job_queue 新 kind，逐图交 VL 推理），结果写 tag 表 **origin=custom**（name=任务标签名，value=推理输出短文本，带 model/taskId 溯源）；全部自定义标签自动出现在搜索复合过滤与标签筛选 UI（tag 表已接过滤链，按 origin=custom distinct names 动态列出）。推理走 inline prompt 通道（对齐 dam-ai /v1/tagging inline 实验通道设计：用户 prompt + 输出格式约束 + 归一解析）；prompt 注入模板化变量说明（图内含什么/输出长度/多值分隔约定）
- 验收：1 万图打标断点续跑；进度可见；失败列表明细；自定义任务：建 2 个任务跑 10 图 → 标签入库 origin=custom → 搜索过滤面板出现并可用；删除任务不清历史标签（可选联动清理）

### M4 索引与搜索（2 周）✅ D5 已落地（reports/D5-vector-search.md）
- ~~embedding 队列（同 job_queue 机制）：embed_image 批量 → LanceDB~~ → 双塔 embed 队列（svc-pipeline/embed_queue.rs，kind=embed:&lt;model&gt;）→ sqlite-vec 分表；断点续跑/3 次重试/取消/幂等
- 三查询命令：search_by_text（siglip 文本塔）/ search_by_image·find_similar（siglip 图像塔）/ find_duplicates（dinov3，分带 near/variant，并查集聚组）；复合过滤（标签/目录/格式/尺寸/日期——颜色过滤仍归 M6 classic 卡）
- embModel 路由：provider.spec().name → v_&lt;model&gt; 分表，无表/维度不符明确报错；换模型老向量保留，重新嵌入走 enq_all
- **图片集导出（裁决 2026-10-13）**：标签/查询结果一键导出，用户可选**原图或缩略图**（256/1024 档）；打包形式：拷贝到目录 / zip（可选附 manifest.csv：文件名/标签/尺寸/原路径/assetId）；文件名冲突自动加序号；reference 模式从 absPath 拷贝，missing 跳过并出缺失清单；后台任务（job_queue kind="export"）进度+完成通知+可取消。入口：批量操作条「导出」+ 标签面板「导出此标签全部」
- 搜索 UI（✅ SearchView.tsx）：文本搜图、以图搜图（结果上「找相似」）、复合过滤面板、查重分带展示、双塔索引状态条与「构建向量索引」
- ~~颜色过滤用 classic `/v1/classify` 主色结果（有 sidecar 时）~~ → 独立 classic 卡（后续）
- 验收（✅ 全过）：10 万级冷搜索 <1s（实测 40ms）；embModel 标识随查询路由，禁止跨模型混算（两表共存只命中当前表测试）
- 前置裁决（✅ 已落地）：libraryRoot 与 appData 分离（Settings 可配）；网格缩略图 asset protocol 直读 256 档；详情 1024 懒生成 +「查看原图」系统看图器

### M5 匹配去重（1 周）✅ D7 已落地（reports/D7-dedup-verifyall.md）
- ~~pHash 汉明距离 ≤8 判定近似重复~~ → 裁决 2026-10-13：单级 cos 判定，pHash 仅作资产表辅助证据。精确重复 = 同 fileHash（SHA-256）分组（导入器入口已去重，组仅来自软删重导入/跨库合并等异常路径）；近似重复 = 去重塔向量直查分带 near（cos ≥0.98）/ variant（[0.90,0.98) 疑似变体·系列图/连拍），并查集聚组
- 重复处理 UI（✅ DedupeView.tsx）：三分带 tab（精确/近重复/变体）+ 回收站；逐组审阅勾选保留（默认保首张，至少保留一张）→ 单组/当前分组批量软删除（deleted=1）
- 软删可恢复：回收站面板单张/全部恢复；软删资产即刻从列表/搜索/查重/统计消失
- 验收（✅ 全过）：注入 10 组已知重复组（含 5 near + 5 variant + 10 干扰项）100% 召回、零误报；软删→恢复回路幂等可重现；verify_all.sh 链尾全链路（清库→建库→导入→mock 打标→mock 嵌入→三搜索→查重→软删恢复→清理）全绿且中断重跑幂等

### M6 打包发布（1 周）
- 安装包：mac (dmg) / win (nsis)；自动更新（tauri updater + Gitea release）
- 无 GPU 机器 10 分钟可用验收（对齐开源硬性验收）
- README（中英）、截图、quickstart
- 验收：干净 mac/win 虚拟机安装 → 导入 → 搜索全流程通过

## 5. 与 souJpg 云 DAM 的关系（PLG 漏斗）

**产品定位（裁决 2026-10-13）**：桌面版面向**各行各业有多量图片需要管理的用户**（电商/房产/制造/设计/医疗等）——行业图片资产本地化管理工具，核心价值 = 行业定制的打标维度 + 高效搜索/去重/批量操作。不同行业需要不同标签体系：自定义提示词打标（D10）是产品基石而非附加功能，行业模板库与受控词表建立在其上。

- 同步入口：设置页「连接 souJpg 云 DAM」— **可见但灰置**（MVP 后实现），选择性同步（按项目/文件夹），字段同名直通
- 转化钩子只在两个断点：协作/审批/分享、多端访问。算力不是钩子（推理可插拔）
- 同步的资产在云端 searchable=0（企业资产永不公共搜索）

## 6. 与 dam-ai / 云 DAM 代码的复用关系

| 复用物 | 来源 | 方式 |
|--------|------|------|
| 推理三引擎 | dam-ai 仓 | sidecar 二进制或远程 endpoint，不复制代码 |
| 资产模型字段 | images_meta/damService.py | 字段名照抄（§3 表），语义一致 |
| 审批状态机语义 | damService.py submitForReview/approveAssets | 桌面版不实现，仅预留字段 |
| VL 编目管线概念 | vlUnifiedFieldMapper | 概念平移（标题/描述/标签三合一模板） |

## 6.1 桌面版 v1 范围裁剪（裁决 2026-10-12）

桌面版 v1 **不做**：审批流（单用户无意义，approvalStatus 恒 approved）、团队/席位（team_member 不进桌面版）、Brand Kit（AI 编辑在 dam-ai 之外，桌面版无 AI 绘图）、计费（闭源层）。desktop 是单用户产品：导入/打标/搜索/去重 + 选择性同步入口（灰置）。

## 6.2 v1.x 候选功能池（2026-10-13 盘点后按行业定位重写，未排期——后续逐个裁决是否排卡）

定位依据见 §5：面向各行业多图用户（电商/房产/制造/设计/医疗等），核心价值 = 行业定制打标维度 + 高效搜索/去重/批量操作。已排期不在此列：path 标签（D8）、provider 预设表单（D9）、自定义打标（D10）、图片集导出（D11）。

**P1 — 行业化核心（建立在 D10 自定义打标之上）：**
1. 行业模板库（打标模板包）：预置电商（颜色/款式/材质/场景）、房产（户型/装修风格/朝向）、汽车（品牌/车型/角度）、食品（菜系/食材/摆盘）等行业 prompt 模板包，一键启用后可微调——零 prompt 门槛获得行业标签维度；模板格式与 dam-ai registry/tasks.d 对齐（可互相导入），内置包随应用分发、用户自建包本地管理
2. 受控词表/枚举标签：自定义打标任务支持「枚举约束」——标签值限定在用户给的候选列表内（如颜色=色卡枚举、房型=两居/三居），prompt 注入候选清单 + 模型只从中选；搜索过滤可靠性远高于自由文本，是行业标签体系可运营的前提
3. 批量元数据编辑：表格化多选编辑 title/description/tags（手动修正 AI 结果、批量追加/替换标签、批量改星级）；行业用户图量大，AI 打标后的人工校对是日常流程
4. PDF/设计稿预览检索：行业图库常混 PDF（合同/图纸/设计稿），表结构 §3.3 已预留（resource role=page）；v1 先做 PDF 页缩略图 + 按页检索

**P2 — 通用管理（个人/行业通用）：**
5. 相册/集合（Album）：行业语义=项目/客户/批次分组；跨目录策展，搜索结果"加入相册"；新表 album + album_assets，成本低
6. 重复/相似清理向导：D7 pHash 标记之上的用户视图——相似组并排 → 勾选保留 → 批量处理；素材库/电商 SKU 多角度图堆积是行业同样常见的痛点
7. 回收站：deleted 字段已有，补回收站视图 + 恢复 + 原地模式可选移文件入系统回收站；批量操作的安全网
8. 保存的搜索（智能相册）：查询条件存为动态集合（"客户提供+未打标"），新导入自动进入；D5 查询管道薄封装
9. 存储统计：按目录/年份/格式体积分布
10. **批处理管道（构想 2026-10-13，高价值差异化，已排卡 D12，插件体系详见 soujpg-product-dam skill）**：动态搜索圈定图片集 → 定义多步处理管道（如 去水印 → 高清化 → 智能裁剪）→ 全量批量执行。**每个功能 = 一个插件**：插件声明 {id, name, 行业分类, 输入/输出, 参数 schema, 引擎依赖}；一个管道可选多个插件串联；每个行业插件内置多个处理器实现 + 推荐管道模板（如电商插件：去水印+主图裁剪+背景规范+尺寸适配一键套用）；**需要大模型的插件给出配置选项**（复用 D9 provider 预设表单模式，插件级持久化，未配置显示"待配置"引导）。执行前单图预览效果；走 job_queue（kind="process"）后台批量 + 进度/取消；**结果不覆盖原图**——衍生图写新文件并入库为新 asset（parentAssetId 血缘字段已预留 + 处理管道溯源），可继续打标/搜索/导出；引擎接入：本地 ONNX（GPU 加速）或外部 OpenAI 兼容图像编辑 API/ComfyUI 工作流，与 provider 可插拔原则一致。与 D5 搜索、D11 导出组合成「搜 → 处理 → 出」完整工作流闭环。工程量大（处理器生态 + 引擎接入），建议 v1.x 后期或 v2 首个攻坚项。**桌面版免费/付费分界（裁决 2026-10-13，仅指 dam-desktop 产品线，与云 DAM 无关）：免费版处理结果只导出到本地目录（不入库）；付费版处理结果自动入库 + 血缘（parentAssetId 衍生资产、可再打标再搜索）**——入库血缘是桌面付费版区别于修图工具的核心价值。免费版导出附带处理记录 sidecar json（源文件/管道/参数/时间戳）

**P3 — 垂直行业增强（按需）：**
10. ~~EXIF 过滤（摄影/电商拍摄行业：相机/镜头/焦段/ISO）~~ → **已实现（EXIF2 / 卡 t_7d9bb323，2026-10-16）**：解析 kamadak-exif（JPEG/TIFF + PNG eXIf / WebP EXIF chunk，HEIC 未覆盖）；派生列 takenAt/cameraMake/cameraModel/lensModel/iso/focalLenMm/aperture/shutterText/orientation + 画幅 aspectRatio/aspectClass；检索 `/api/library/assets` 支持 takenFrom/takenTo、cameraModel、lensModel、isoMin/isoMax、aspect、sort=takenAt；导入即解析（默认开）+ 存量回填（job kind=exif，64/批、重试、断点续跑、retry-failed、id 区间）；实测报告 reports/EXIF2-metadata.md
11. 截图/图纸 OCR 文字搜索（PaddleOCR ONNX；含合同、工单截图场景）
12. GPS 反解地点标签（外拍/地产踩盘行业）→ **部分实现（EXIF2，2026-10-16）**：GPS 解析 + 两档粒度（只存坐标 / 只存地名）+ 可插拔逆编码 provider（默认未配置即不做，无内置 key）+ 导出清单 hasGps 列与 strip_gps（JPEG 去 GPS IFD）均已落地；**默认关闭（off），隐私红线：不解析不存储不落日志**。未覆盖：在线 provider 端到端实测（需用户自有 key）、PNG/WebP chunk 级剥离、地名聚类/地点标签体系

**v2：**
13. 人脸/宠物聚类（人像/宠物品类电商）
14. 视频支持（表结构已预留，§3.3 路线）
15. 幻灯片放映（低优先，个人向体验）

建议首批裁决：行业模板库 + 受控词表（行业定位的立身之本，直接建在 D10 上）、批量元数据编辑、相册、重复清理、回收站。

## 6.3 合规判定引擎（C 链，2026-10-16 排卡；详设与排期见 damdesk-compliance-rules-engine-plan）

来源：自定义打标（D10）泛化为「可定制判定」——规则引擎 = 处理插件（packs/plugins）声明 + compliance job +
官方修复插件 + 反馈闭环；同时吸收 D12 批处理管道的一般形态（管道是一般形态，compliance 是其判定特例）。

- **packs 四文件整包原子装载**（数据不是代码）：`packs/<packId>/{taxonomy,rules,pipeline,report}.yaml`
  一起生效，缺一即装载失败（无半包）。同刻只激活一个 pack，切换即整体替换；已产判定按
  packId+ruleId 归属保留（换包不丢历史）。v1 来源 = 内置 packs（编译期内嵌、运行时 YAML 解析）+ 本地目录导入。
- **rules.yaml 六字段**：id / name / executor(classic|local_model|vl) / evidence(image_only|external_data；
  external 规则 v1 直接报「不可自动判定」) / check{detector,params} / fixer{type,tier(guaranteed|best_effort)} /
  on_fail(pass|auto_fix|retry_then_human)。打标型规则（executor=vl + outputs）不打合格/不合格，只填 taxonomy 字段。
- **pipeline step 双语义**：judge 步骤 = 检测 → fail 触发 fixer → 复检（guaranteed 修完必过；best_effort
  复检最多 recheck_max 次，仍败转人工）；do 步骤 = 无条件变换（resize/格式转换/压缩/水印/重命名），不判定。
  同一编排器、同一插件注册表；短路用步骤级 depends_on 声明（依赖规则 fail → 本步 skipped）。
- **判定数据模型（migration 0011，只加不改）**：`rule_verdict`{id, packId, assetId, jobId, ruleId,
  result(pass/fail/skipped/pending_human), score, evidence json, fixAction, fixTier, recheckResult, ts}；
  `rule_feedback`{id, packId, assetId, ruleId, machineResult, humanResult, reason, ts}（review「误判」按钮写入，
  v1 只落库 + CSV 导出）。计划文档中的 imageSN 在本地 = assets.assetId（本地主键即 SN 等价物，全库沿用 assetId 命名）。
- **deferral chain**：元数据(~0ms) → classic(毫秒) → local_model(ONNX 秒级) → vl(远程，复杂 prompt 最后调) →
  人工队列。已有标签只做预筛加速，**合格结论只能来自本次执行的检测器输出或人工**（判定可复现）。
  修复产物 v1 不入库（复检用临时目录；导出+sidecar 免费，入库血缘付费——D12 分界沿用）。
- **local_model 档已落地（C3，2026-10-16）**：`engine-segment`（L1 新 crate）+ u2net ONNX FP32
  （~168MB，rembg 同源；`kind: segmentation` registry 条目，随安装包内置分发 + model_store manifest
  兜底下载）。检测器 `fg_ratio`（320² 网格掩码占比，阈值 0.5 min-max 归一，主主体=最大连通域）；
  修复器 `smart_crop`（首个 guaranteed 插件：主主体 bbox 中心搜索目标占比裁剪窗，确定性；
  fixer.params {min,max} 声明目标区间）。模型未下载 → skipped(model_not_downloaded)，
  web 容器（无 local-infer）→ skipped(executor_not_available)。
- **修复器/do 变换全家桶已落地（C4，2026-10-16）**：`engine-restore`（L1 新 crate，
  realesr-general-x4v3 超分包内置 + big-lama FP32 重绘走 model_store）+
  `core-media::{encode_rgb_to, watermark_text}`（纯 Rust 编码底座，双后端同语义；
  内嵌 Noto Sans SC OFL 字体）。插件注册表 8 件：replace_bg(guaranteed)/upscale/
  inpaint(best_effort) + convert/compress/watermark/rename(classic do 型)。
  修复产物不入库（outDir + 导出，D12 分界）；模型缺失 → fixer 转人工(model_not_downloaded)/
  do 步 skipped；插件签名带 PluginOrigin（rename 模板 {origName}/{assetId}/{w}/{h}/{ext}）。
  详见 reports/C4-restorers-do-transforms.md。
- **判定通道唯一化（IP-K16 + N18，2026-10-22/23 落地）**：判定/修复**只有一个执行通道** ——
  处理插件**子进程**（`docs/industry-pack-protocol.md` v1.1 的 `judge`/`repair`，宿主只做映射与落库）。
  实现分层：端口 `svc_pipeline::PackJudge`（L2，同层禁互引所以只声明端口），实现在
  `app-web::compliance_api::PluginRuntime`（L3 用 `pack-host` 装配网关 + 子进程）。
  - **没有引擎开关**：settings `compliance.engine` / 环境变量 `DAM_COMPLIANCE_ENGINE` /
    `/packs/activate` 全部下线（IP-K16）；残留设置项由迁移清掉，判定行 `packId` 统一到插件身份 id。
  - 用户侧的入口只有**规格**（选规则 + 阈值 + 动作链 + 输出）；插件是「能做什么」，
    规格是「做成什么样」，装/卸/切版本在插件详情页（N18 起错误带 `code` + `hint` + `action` 直达该页）。
  - 插件失败 ⇒ 包内每条规则一行 `undetermined` + `evidence.reason`（`plugin_timeout`/`plugin_crash`/
    `plugin_unavailable`/`plugin_rule_error`/`plugin_protocol_error`），**绝不冒充 pass/fail、绝不让宿主退出**。
  - 组级判定（`per_group`）不在插件通道范围（协议 v1.1 的 judge 为单资产调用）→ `scope=group` 直接 400。
  - 落库：migration `0026 packs`（包身份账本）+ `0027` 判定行 `packVersion`/`pluginHash` 两列 +
    `0030 plugin_unify`（老 `ecommerce-taobao` 行改键到插件 id、清引擎设置项）。
    判定行 `packId` = 插件 manifest id（`com.soujpg.pack.ecommerce`）。
  - `pipeline.yaml` 的 `steps`/`recheck_max` 只是**元数据**（读面展示/装载校验），宿主不据此调度：
    执行序、修复策略与复检轮次全由插件决定（`fix_policy`/`max_rounds` 在包内）。
    **不要在这两个字段上再新增语义**：新流程写进处理插件。

## 7. 测试与验收

- Rust 单测：importer/dedup/search 核心逻辑；tag/pHash 解析
- 集成测试：mock 推理 endpoint（本地起假 /v1/embeddings + /v1/tagging）跑全链路
- E2E（人工）：干净机器安装→导入→打标→搜索→去重全流程录像
- 性能基准：1 万图导入、10 万级搜索 <1s（CI 跑不动，release 前 manual benchmark）

## 8. 风险

| 铁律 | 说明 |
|------|------|
| sidecar 体积 | 模型不内嵌，首启引导下载（国内镜像）或引导配远程 API |
| LanceDB crate 成熟度 | ~~备选方案 sqlite-vec；M0 时 spike 验证~~ → **已裁决（D5，2026-10-13）**：选 sqlite-vec（100K×512 KNN 40ms、零依赖打包）；≥百万级重开 spike（reports/D5-spike-decision.md） |
| tagger 无 GPU | 降级链：sidecar GPU → 自配公有云 VL → 离线（无自动打标） |
| 打标质量一致性 | 模板与 souJpg 生产模板同源（deploy/damai-tasks 思路），结果字段一致 |

## 8.1 桌面版无账号体系

桌面版**无登录**（单用户本地产品）。souJpg 账号绑定（用于选择性同步）是后续版本功能，MVP 不做登录 UI。

## 9. 推理架构（裁决 2026-10-12 二次简化：单进程内嵌，去 Python 依赖）

**易用性优先**：默认路径 = 装 → 拖文件夹 → 直接能搜，零配置。放弃 sidecar/companion 方案（装两个包对用户太复杂）。

**分工**：React TS 只做 UI；一切推理在 Rust core 进程内（Tauri 惯例：重活 core、UI 薄）：
- embedder：ort crate + **SigLIP 2（siglip2-base-patch16-224，86M，dim 768）ONNX 内嵌**（图+文双塔，vision/text 两个 onnx 文件 + tokenizer.json），模型首启静默下载（fp16 ~215MB×2 塔，国内镜像 hf-mirror），CPU 约 30-60ms/图。选 SigLIP 2 而非 CLIP 的硬理由：①文本侧 Gemma tokenizer（256k 词表）原生支持中文查询（CLIP BPE 英文中心，中文需翻译层）②同尺寸检索质量优于 CLIP。embModel 标识 = siglip2-base-p224-onnx，dim 768
- classic：主色/调色板 纯 Rust 算法
- pHash/SHA-256：纯 Rust
- tag client：唯一可选增强——设置里「启用 AI 打标」→ 配云 VL API（OpenAI 兼容）；无 vLLM/GPU 依赖

**为何不在 JS 层推理**：WebView WASM 单图几百 ms + WebGPU 跨平台 webview 兼容碎，万图导入撑不住；Rust ort 快一个量级且后台线程不卡 UI。

**Provider 架构（对齐 dam-ai 的 registry/loader 模式，2026-10-12 裁决）**：
Rust 侧平移 dam-ai 的三层结构，未来加云 embedding/多模态服务 = 加一个 Provider 实现 + 一条 registry 条目，零改核心：
- `registry/models.yaml`（同哲学：name→配置的单一寻址入口；dims/modalities/normalized/preprocess 每模型锁定，preprocess 变更必须重嵌入）
- `ModelSpec` 结构体（解析 yaml）
- `EmbedProvider` trait（对应 dam-ai LoadableModel）：`load()` / `embed_images(&[ImageInput])` / `embed_texts(&[&str])` / `ready()` / `spec()`
- v1 两个实现：`OnnxSiglipProvider`（内嵌默认，ort + siglip2-base-p224）/ `OpenAiCompatProvider`（OpenAI 兼容 /v1/embeddings，支持 image_url+text——可接 dam-ai server、dashscope、JinaCLIP API 等任何多模态 embedding 云服务）
- settings 选择 active provider（按 spec.name）；embModel 存 spec.name，禁混算铁律由「向量表按 name 分表」天然满足
- 同一 trait 扩展打标侧 `TagProvider`（v1 仅 `OpenAiCompatTagProvider`，本地 tagger 留接口位）

**dam-ai 关系（参考不依赖）**：接口 schema（OpenAI 兼容）、打标模板设计照搬；运行时不进 desktop。dam-ai 继续服务 self-hosted/server 形态。云端同步时云端管线自带重嵌入；本地向量模型差异由 embModel 路由兜底（禁混算铁律不变）。

**降级链（简化后）**：本地 ONNX（默认，零配置）→ 纯离线（元数据/pHash/文件名/**EXIF 检索（已实现：拍摄时间/相机/镜头/ISO/画幅，见 §6.2-10 与 reports/EXIF2-metadata.md）**）→ 云 VL 打标（可选增强）。

## 10. 待拍板（收敛后）

- 首发平台：mac 先（用户主力 mac）/ win 先（企业用户多）→ 建议 mac 先
- 模型分发：首启下载 vs 安装包内置一部分
- CI（Gitea Actions runner 是否有 mac/win）
- （已裁决：sidecar 分发 = dam-ai companion 独立包，不随 desktop 捆绑）


## 11. 开发环境（裁决 2026-10-12：容器开发，不在宿主机装工具链）

- 宿主机 Ubuntu 20.04 缺 webkit2gtk-4.1（tauri 必需），且遵循容器原则 → 一切构建/测试在 dev 容器内
- **dam-desktop-dev 容器**：Debian bookworm-slim + rustup stable + node + pnpm + libwebkit2gtk-4.1-dev/libgtk-3-dev/libvips-dev/libxdo-dev/libssl-dev/xvfb；定义在仓内 `dev/Dockerfile` + `dev/docker-compose.yml`
- 卷：仓库 bind mount；`CARGO_TARGET_DIR` 与 cargo registry 走 docker named volume（NVMe——/data 是机械盘，禁放编译产物）
- 网络：extra_hosts `host.docker.internal:host-gateway`，npm/crates 走宿主代理 `http://host.docker.internal:11700`；推 Gitea 在宿主执行（容器内不推）
- GUI 分工：gpu0 headless——容器内 cargo build/test + xvfb 冒烟；React UI 开发用 vite dev server（映射 5173，浏览器访问 + invoke mock 层）；真机窗口体验在 Mac 跑 tauri dev

## 12. W 链 — dam-web（web 先行，2026-10-14 起）

裁决背景与全链拆分（W0-W7）见总计划：
`~/.hermes/profiles/soujpg/plans/dam-web-implementation-plan.md`（gpu0/gpu7 同步副本）。

- R5 落地：本仓即 dam-web 主仓 —— 新增 `crates/app-web`（axum 壳）与 `web/`（Vite+React18+Tailwind v4 暗色前端），既有 crates/src/app-tauri 原样保留（W7 才动）。
- W0（本节）：monorepo 骨架 + app-web（/healthz、/api/app-info、web/dist 静态服务、core-storage open/migrate）+ `deploy/` Dockerfile/compose。
- 后续 W1-W7 卡逐张落 reports/。

## 7. W7 落地记录（2026-10-15 done，详见 reports/W7-desktop-shell.md）

**交付**：R3 终态落地 —— Tauri 壳内嵌同一个 app-web（`127.0.0.1` 随机端口 + 一次性
token 引导 `/auth/boot` → HttpOnly 会话 cookie，前端 bundle 零改动）；旧 invoke IPC
命令层（commands_*/state.rs ~1900 行）与旧桌面前端（根 src/ + jsdom 测试）整体下线；
`app-web/local-infer` feature 收口 R6（桌面 ONNX 内嵌、容器 HTTP-only，镜像不背
ort/模型）；桌面首启默认 provider=local（`seed_defaults_opts` 分流），离线四态
（local/远程 dam-ai/dashscope·zhipu·Ollama 预设/自定义 base_url）探测降级链原样保留。
附带修复 W3 遗留 models_root 隐式错传（factory 根契约为 appData 式根，web 从不触发、
桌面首次真正走到——已按契约显式化 `AppState.models_root`）。

**测试栈**：`pnpm test` = `pnpm -C web test`（vitest，selection 11 例）；web/
新增 vitest（node 环境）；cargo workspace 全绿（真模型 e2e 含）。check_layers 新增
W7 豁免 app-tauri→app-web（L3 壳组合）。

**部署**：容器路径行为不变（token=None 鉴权关闭）；gpu7 `damdesk` 容器（原名 dam-web-api，2026-10-15 按命名约定统一为 damdesk）已重建部署
（healthz ok）。mac 构建冒烟：`pnpm tauri build` + open 存活（资源含 web-dist + 双模型）。

### W7+ — 原生能力补位（web 拿不到的就借 Tauri）

原则：**双形态共享同一份 web/dist**，凡是浏览器沙箱给不了的能力（本地目录绝对路径、
系统通知、在访达中显示…）不在 web 里硬编，而是「桌面壳加一项最小原生能力 + 前端运行时
探测，探测不到就不渲染入口 / 走手填回退」。禁止无条件 invoke —— 容器 `damdesk` 形态没有
Tauri 运行时，无条件调用必崩。

首个用例：导入向导「选择文件夹…」（`dialog:allow-open`）。

| 层 | 改动 |
|----|------|
| 壳 `app-tauri` | `tauri-plugin-dialog` + `.plugin(init())`；`capabilities/default.json` 只给 `dialog:allow-open`（fs/shell/http 一律不开）；壳仍零业务逻辑 |
| 前端 `web/` | `src/lib/native.ts`：`isDesktopShell()` 探测 `__TAURI_INTERNALS__`、`pickFolder()` 走 `plugin:dialog|open`（手写 IPC，不给共享 bundle 引 Tauri JS 依赖），非桌面形态返回 null |
| UI | 向导第一步「图片目录」旁加按钮，仅 `isDesktopShell()` 为真时渲染；取消/失败 → 手填路径兜底（失败文案落 `scanErr`） |

导入链路契约不变：选中的绝对路径回填输入框 → `POST /api/import/scan {path}` 由服务端递归
扫描（原地索引 / 复制入库仍由向导第二步决定）。后续同类需求沿同一模式加，一次只开一项最小权限。

**打包坑（已修）**：Tauri 资源映射 `"../../web/dist/**/*": "web-dist/"` 会把匹配到的文件
**抹平**拷进 `web-dist/` 根，丢掉 `assets/` 子目录 → index.html 引用的 `/assets/index-*.js`
404 → `ServeDir` 的 SPA fallback 用 index.html(text/html) 兜底 → module MIME 不符，React
永不挂载 = **窗口白屏**。修法：补显式映射 `"../../web/dist/assets/**/*": "web-dist/assets/"`。
新增前端产物子目录时（如 worker/字体目录）都要照此补一条。
