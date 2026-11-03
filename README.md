# DAMDesk

**AI-Powered Image Operations** — 单机优先的图片治理工作台：本地打标、规则校验、批量处理、标准化交付。

[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux%20(self--host)-lightgrey.svg)](#安装与运行)
[![Rust](https://img.shields.io/badge/backend-Rust%20%2B%20axum-orange.svg)](crates/app-web)
[![Tauri](https://img.shields.io/badge/desktop-Tauri%202-24C8D8.svg)](crates/app-tauri)
[![Docker](https://img.shields.io/badge/self--host-docker%20compose-2496ED.svg)](deploy/docker-compose.yml)

DAMDesk 不把图片当相册，而当**生产资料**：图片是原材料，要能被打标、被校验、被批量处理、被标准化交付。
导入即建库，AI 打标、规则校验、批量处理、导出全在本机跑完；元数据、判定记录、产物清单都落在本地 SQLite，
断网照常可用，没有账号，也不上传任何图片。

单用户、单机、零运维。多租户协作 / 审批签核 / 计费不在本产品范围内。

---

## 能做什么

| 能力 | 说明 |
|---|---|
| **AI Tagging** | 多模态模型打标：模板任务 + 自定义任务，结构化输出拍平成离散标签，两段式人工确认后才入库 |
| **Quality Validation** | 规则引擎校验（`rules.yaml`），三态判定 + 置信度 + 低置信度人工复核，可就地改判 |
| **Batch Processing** | 队列化批处理：打标、修复、变换、去重、导出；64/批、断点续跑、失败按资产记 `attempts`/`lastError` |
| **Processing Plugins** | 一个能力 = 一个可安装插件（`.dampack`），行业只是插件携带的属性，换行业 = 换插件，不改宿主代码 |
| **Automated Delivery** | 导出目录 / zip + `manifest` + 判定报告，交付时自带证据 |

### 谁会用到

- **电商**：商品图批量打标（品类 / 规格 / 材质）、白底主图合规校验、标准化导出给渠道
- **制造**：型号 / 规格归档，尺寸与包装属性统计，交付前一致性检查
- **房产中介**：房源图统一命名、室内 / 室外分类、入库前合规审、失联文件记录
- **设计 / 品牌**：素材分级（色标 / 自定义质量标签）、重复素材清理、按活动渠道分发
- **个人 / 小团队**：本地图库 + AI 打标 + 去重，数据不出本机

---

## 主流程

```
① 导入    拖文件夹 / 选目录 → 库根登记 → 缩略图与向量生成
② 打标    建任务（模板或自定义 prompt，写明输出格式）→ 入队 → 模型跑 → 待确认
③ 确认    逐条 / 批量确认（可改值）→ 写入正式标签表
④ 找图    目录树 / 标签 / 未打标 / 失联 / 无法预览 多维叠加筛选；文搜图、以图搜图
⑤ 校验    选中图片「送去处理 →」选能力 + 方案 → 跑一次 job
⑥ 处理    修复（超分 / 抠图 / inpaint）、变换（尺寸 / 格式 / 水印）、去重（pHash + 向量）
⑦ 导出    选范围（筛选结果 / 选择集）→ 目标目录 + manifest → 任务中心看进度
```

关键交互约定：

- 筛选条件与 URL 同步，刷新 / 深链 / 后退都能复现同一次筛选，芯片可逐个摘除。
- 搜索页有 query 走向量检索；**只有过滤条件也能浏览**，计数与图库同口径。
- **图库 / 搜索 / 检查器是纯资产域**：只有固有属性和标签、色标、拒选、目录，没有判定或 job 概念；
  判定结果、人工改判、按结果导出全部收敛在任务中心（L1 列表 → L2 本次执行 → L3 图级明细）。
- 命令面板 `Ctrl / ⌘ + K`：跳页面、跑动作、找筛选。

---

## 核心设计

### 标签是离散字符串，不是表结构

- 打标结果一律输出数组，每个值扇出一条标签；`tag` 表只有 `value` 一列，唯一键 `(assetId, value)`。
- 结构化属性拍平成 `任务名-值`：`材质-棉`、`场景-室内`。值内多词用下划线（`尺寸-滚筒_10KG`），
  `-` 只分隔层级，`_` 只连接段内词。
- 值必须离散：连续量先分桶（`尺寸-4k`、`年份-2024`），v1 不做数值范围查询。
- 多级值深度 ≤3（`尺寸-滚筒-10KG`），只有叶子落库，父级是视图节点，计数由前缀聚合得出，不建层级表。

这样做的收益：**行业定制属性 = 新任务 = 新标签前缀，零 schema 变更**。
代价：数值聚合（均值 / 求和）做不了，放导出层。

### 两段式打标确认

任务跑完不等于标签入库：结果先进 `tag_proposal`（提案），在「待确认」页逐条或批量确认才写正式表，
人工修改优先于模型输出。模型质量不可全信，人要对自己库里的标签负责。

### 三个正交标记，永不合并

| 标记 | 语义 | 写入方 |
|---|---|---|
| 拒选 `reject` | 「这张不要」 | 人工（检查器 / 工具条 / X 键） |
| 色标 `colorLabel` | 团队自赋分类 | 人工（检查器） |
| 标签置信度 `score` | 模型把握多大 | 模型（永不覆盖人工标记） |

产品**不做星级评分**：质量怎么算由行业自己定义，用定制标签表达（如 `质量-优`、`瑕疵-轻微`）。

### 判定与资产分域

资产域只描述「这张图是什么」，任务域只描述「这次跑出了什么结论」。两域不交叉污染：
一份判定结果永远归属于某次 job 执行，可以追溯、可以改判、可以按 job 导出。

---

## 处理插件

插件是**独立子进程**，用 stdio 上的行式 **JSON-RPC 2.0** 跟宿主说话；宿主只提供**能力网关**
（几何 / 分割 / OCR / VL / IO / 报告）。母版：*插件输入一批图，输出每图一行结果（处理产物和/或判定结论）*。
判定、修复、处理的步数、顺序、分支全在插件代码里，宿主不解释流程；协议 v2.0 只有一个执行动词 `run`。

```
manifest.json            插件身份：id / 版本 / 需要的宿主能力 / outputs
plugin/                  流程代码（判定步数、顺序、分支、修复策略都在这里）
resources/rules.json     规则清单 + 报告列名 + 低置信度阈值
resources/taxonomy.json  受控词表（打标只往这些值里落）
```

官方五包（`packs/plugins/`，均为 `api_version = 2.0`）：

| 插件 | 输出 | 说明 |
|---|---|---|
| `ecommerce` 电商主图合规 | verdict + artifact | 白底商品图检测、OCR、几何合规、VL 判定 |
| `realestate` 房源照片合规 | verdict + artifact | 入库前房源图审查，判定报告随交付 |
| `imaging` 图片处理 | artifact | 缩放 / 裁剪 / 转格式 / 压缩 / 水印 / 改名 |
| `segment` 抠图换背景 | artifact | u2net 主体分割 + 换底色 |
| `upscale` 超分 | artifact | Real-ESRGAN 2x / 4x |

边界（CI 强制断言）：**批量打标、查重去重、EXIF / OCR、向量索引与语义检索**是所有人都要、实现唯一、
只需开关和参数的一档，**永远内置**，不包成插件；换模型 / 换 provider 是配置，不是插件。

插件协议全文：[`docs/industry-pack-protocol.md`](docs/industry-pack-protocol.md)，
进程模型与实现细节：[`docs/rust-python-plugin-process-model.md`](docs/rust-python-plugin-process-model.md)。

---

## 安装与运行

### 自托管（容器，最快上手）

```bash
docker compose -f deploy/docker-compose.yml up -d --build
curl 127.0.0.1:8787/healthz     # {"status":"ok","db":"ok"}
```

- 一个 `app-web` 进程同时供应前端 bundle 与 JSON API，默认端口 `8787`，数据在容器 `/data` 卷。
- 镜像**编入 local-infer**（`--features local-infer`），与桌面版能力面对齐，官方五包都可装。
- 模型**不内置**：挂 `DAM_WEB_MODELS_ROOT` 指向 ONNX 权重目录，或在应用内下载；
  embedding 也可改走远程 OpenAI 兼容 provider。
- `deploy/docker-compose.yml` 里的构建代理 / 语料挂载是内网开发默认值，自托管时按需删掉。

### 桌面（从源码构建）

```bash
pnpm install
pnpm -C web install
pnpm tauri build        # 产物：macOS .app/.dmg、Windows NSIS .exe
```

构建前置：Node 22 + pnpm、Rust stable、libvips 开发包（`libvips-dev` / `brew install vips`）。
打包前需把 ONNX 权重与内置 Python 运行时 stage 进 `crates/app-tauri/resources/`（各目录 `README.txt` 写了来源与命令）。
**当前未提供官方预编译安装包**；macOS 侧未签名 / 未公证，自行构建后首次打开需右键 → 打开。

### 本地开发

```bash
pnpm -C web install
cargo run -p app-web      # 127.0.0.1:${DAM_WEB_PORT:-8787}，数据在 ${DAM_WEB_DATA:-./data}
pnpm -C web dev           # vite :5180，/api 反代 127.0.0.1:8787（VITE_API_TARGET 可改）
```

### 系统要求

```
macOS      14+（Apple Silicon）
Windows    10 / 11 x64（WebView2 运行时，安装包自带引导）
Linux      仅自托管容器形态
内存       8 GB 起（16 GB 建议，批量处理 / 向量索引更稳）
磁盘       桌面版安装包约 1.1 GB + 库容量（缩略图与向量约占原图 5–10%）
网络       仅远程打标需要；其余全离线
```

内置模型（随桌面安装包，约 1.1 GB）：SigLIP2-base（text 539M + vision 177M，768 维文图向量）、
DINOv3 ViT-S/16（82M，384 维去重塔）、U2Net（168M 抠图）、PP-OCRv6-medium det（59M）+ rec（73M）、
Real-ESRGAN x4v3（4.6M）。Inpainting（LaMa）不内置，按需下载。

---

## 性能与规模

- 标签：50 万资产 × 15 标签 = **750 万行**，单值筛选 1.3 ms、双标签交叉 86 ms、三标签交叉 124 ms（SQLite 单机）。
- 标签候选：走字典表（distinct 值 + 计数）前缀范围扫描，微秒级，与总行数无关。
- 打标队列：64 / 批，断点续跑，失败按资产记 `attempts` + `lastError` + 重试。
- 导出：目录 / zip + manifest（含判定与来源），进度在任务中心。

---

## 仓库结构

```
crates/core-*      L0 基础：类型 / SQLite+sqlite-vec / hash / libvips / 配置
crates/engine-*    L1 引擎：embed / tag / search / dedup / ocr / segment / restore / jobq
crates/svc-*       L2 编排：import / pipeline / library / sync
crates/app-web     L3 后端：axum，同时供前端 bundle 与 JSON API（桌面与容器共用）
crates/app-tauri   L3 桌面壳：Tauri 2，内嵌 app-web（127.0.0.1 随机端口 + 一次性 token）
crates/pack-host   插件宿主：能力网关 + JSON-RPC 2.0 + 子进程生命周期
web/               前端：Vite + React 18 + Tailwind v4
packs/plugins/     官方五包（插件代码 + 规则 + 词表）
tools/             插件协议一致性套件、launcher 等工具
docs/              产品说明 + 插件协议 + 进程模型
deploy/            Dockerfile 与 compose
```

分层铁律：`L3 → L2 → L1 → L0` 单向依赖，同层禁止互引（`scripts/check_layers.sh` 强制）。

---

## 文档

- [产品说明](docs/damdesk-产品说明.md)：能力、概念、主流程、边界、限制
- [处理插件协议 v2.0](docs/industry-pack-protocol.md)：manifest、能力网关、`run` 语义、报告契约
- [插件进程模型](docs/rust-python-plugin-process-model.md)：谁启动谁、解释器选择、故障冷却

---

## 已知限制

1. 数值聚合（均值、求和、范围比较）不做，标签是离散字符串。
2. 标签 key（任务名）改名不追溯已落标签前缀，只影响新扇出。
3. 完整性报告页、库备份、XMP sidecar 尚未排期；合规反馈闭环在做。
4. 标签总览页与统一候选组件在计划中，当前候选只在输入框内提供。
5. macOS 安装包未签名 / 未公证。

---

## 许可证

[AGPL-3.0](LICENSE)。可自由使用、修改、自建分发；对外提供服务时需按 AGPL 开放你的修改。
