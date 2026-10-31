# DAMDesk 架构（ARCHITECTURE）

> 本文档是**唯一架构文档**：两形态、crate 分层、数据流、存储路径、插件运行时、可审计性与关键取舍。
> 入门与快速上手见 `README.md`；行业包协议的字段/方法权威见 `docs/industry-pack-protocol.md`（本文不复述协议表）。

---

## 1. 两形态一代码（One codebase, two forms）

桌面版与容器版**共用同一份 app-web 与同一条数据管线**，差别只有两处：编译期 feature（`local-infer`）与模型来源。

| 维度 | 桌面（`app-tauri`） | 容器 / 服务器（`app-web` 单跑） |
|---|---|---|
| 进程结构 | Tauri 壳内**同进程**内嵌 app-web（`127.0.0.1` 随机端口 + 一次性 token 引导，随进程退出），WebView2 / WKWebView 渲染 `web/dist` | 单跑 app-web（axum），浏览器直连 |
| 本地推理 | `local-infer` 编入本地 ONNX：siglip2 / DINOv3（embedding）、u2net（分割）、PP-OCRv6（OCR）、x4v3 / lama（重绘） | feature 关闭 → 本地 ONNX provider **明确报错，不静默降级**（`engine-embed/factory.rs`） |
| embedding 来源 | 本地 ONNX（模型落 `{appData}/models`） | `openai_compat` 远程 provider（配置指定） |
| 模型来源 | 三级解析：`{appData}/models/<name>` → 安装包内置 `resources/models` → 兜底 `{appData}`（逐模型回退） | 远程推理服务承载，不落本地模型 |
| 数据面 | 完全一致：同一个 `dam.db`、同一条导入 → 检索 → 判定管线 | 同左 |

设计口径（R6）：差异**只允许**出现在 feature 门控与模型装配层；任何业务逻辑不得感知「桌面 / 容器」而分叉。容器构建不带 ONNX 运行时（二进制体积与许可负担），因此关 `local-infer` 是构建产物属性，不是运行时开关。

---

## 2. crate 分层（L0 → L3，单向向下）

```text
L3  app-tauri / app-web / dev-e2e        装配层：壳、HTTP API、编排入口
L2  svc-import / svc-library / svc-pipeline / svc-sync / pack-host
                                          服务编排 + 行业包宿主内核
L1  engine-embed / engine-tag / engine-classic / engine-jobq / engine-search
    engine-dedup / engine-segment / engine-restore / engine-exif / engine-ocr
                                          算法引擎（可独立单测）
L0  core-types / core-storage / core-hash / core-media / core-config
                                          类型、存储、哈希、媒体原语、配置
```

- **依赖方向**：单向向下、同层禁互引；`scripts/check_layers.sh` 解析各 crate 的 workspace 内部依赖强制检查。`core-types` 是唯一全共享类型层（任何层可用）。
- **pack-host 的层级**：与 `svc-*` 同为 L2，但它**只依赖 L0/L1**，不依赖任何 `svc-*`——它是「能力网关 + 子进程生命周期」的基础设施，被上层装配使用（VL 通道这类跨层能力由 L3 注入端口，见 §5）。
- **存储解耦铁律**：repository trait 定义在 `core-storage`（如 `compliance_runs.rs` 的 trait），SQL 只出现在对应 `*_sqlite.rs` impl；上层只面向 trait 编程，替换存储后端不需要动 L1/L2。任何 crate 不得绕过 trait 直写 SQL。

### 路由分组（app-web）

REST 只画分组，全量清单不在架构文档维护：`/api/{app-info,capabilities,healthz}`、`/api/import/*`、`/api/library/*`（资产/标签/核查/统计）、`/api/search/*`、`/api/{export,dedup}/*`、`/api/models/*`、`/api/compliance/*`（判定与包管理）、`/api/specs/*`、`/api/os/*`、settings 读写。

---

## 3. 数据流

```text
导入 → 打标 → 向量索引 → 检索 → 判定（插件） → 修复 → 导出
```

1. **导入**（`svc-import`）：`storeMode=reference`（原地索引，默认）或 `copyin`（拷入库目录，hash 前 16 位分目录防冲突；源在库内则拒绝，防递归）。每文件幂等：以 `absPath+fileHash+mtime+size` 判定已导入。
2. **打标**（`svc-pipeline` 各队列）：`tag_queue`（VL/规则打标）、`exif_queue`、`ocr_queue`，各自幂等、失败即落账。
3. **向量索引**（`engine-embed` → `core-storage::vecstore`）：sqlite-vec 按 embedding 模型分表（`v_<sanitized>`，登记于 `vector_tables`）。**双塔**：语义塔（siglip2，文搜图）与判重塔（DINOv3，固定用于 dedup，与 active embedder 解耦）；不同模型**禁混算**（维度与语义空间都不同）。进程级注册 sqlite-vec 扩展（首个连接前）。
4. **检索**（`engine-search`）：text / similar / by-image 三入口。KNN + 标量过滤是 **lossy post-filter**（spike 实测 20 取 6），因此过滤语义由调用方**预取候选 id 集**再交给 vecstore，vecstore 只提供原语。
5. **判定**（packs，经 `pack-host` 子进程）：结果行落 job 账本（`compliance_runs`）+ `human_review`（append-only，键 `assetId/schemeId/ruleId`，内容版本变更即失效）。判定字段**不在资产 API 上**（已显式下线，400）——判定是流水账，不是资产属性。
6. **修复**（`compliance_chain`）：按规则的动作链产出修复产物（裁剪/超分/重绘/换底等，经同一能力网关），产物与原资产分离落盘。
7. **导出**（`svc-library::export`）：筛选 → 导出任务（start/cancel/status），一次性动作不设历史页。

---

## 4. 存储与路径

`{appData}` 固定布局（桌面 = Tauri `app_data_dir`，与旧桌面版同布局；容器 = 数据目录挂载点）：

| 路径 | 内容 |
|---|---|
| `dam.db` | 单 SQLite：资产/标签/设置/队列账本/判定账本 + sqlite-vec 虚表 |
| `thumbs/` | 预览缩略图 |
| `models/` | 本地 ONNX 模型（首启下载或安装包内置回退） |
| `dam/packs/<id>/<version>/` | 已装行业包（`.dampack` 解包形态，含包内 `venv/`） |
| `library/` | copy-in 模式原图根（默认；`libraryRoot` 可配置分离，2026-10-13 裁决——**db/thumbs/models 不随库目录迁移**） |

### `missing` 是快照，不是实时值

- **写入口径（失败即标，G5-A）**：仅「文件不在了」（含路径中间段被删/被换）写 `missing=1`；`PermissionDenied` 与其他 IO 错误**不标**（那是 unreadable 语义，不折进失联）。读成功且此前被标 → 自动清回 `missing=0`（文件回来了顺手接回）。
- **新鲜度口径**：stats 响应必须同时带 `missingCheckedAt` / `missingStale`（阈值 `MISSING_CHECK_STALE_SECS = 600s`），过期显示「待核查」——单看 `missing` 无法区分「确实没有失联」和「从没核查过」。
- **核查动作**：显式 `POST /api/library/verify` 全库现场核查（单飞，进行中 409）+ 启动时一次；结果落 settings 表（`missing_check`），跨重启保留。不做 stats 每次现场 stat（核查成本 = 每资产一次 stat）。

---

## 5. 插件运行时（架构核心）

行业包（industry pack）是**任意 Python 代码 + manifest 能力白名单**的子进程插件。宿主侧内核 = `pack-host`（L2），协议权威见 `docs/industry-pack-protocol.md`。

### 5.1 为什么子进程，而不是 WASM / DSL

- **WASM**：跑不了真实算子生态。判定与修复需要 Python 依赖链（图像处理库、自定义模型前后处理），且宿主已有 L0/L1 的 ONNX/经典算法实现——WASM 沙箱既装不下这个生态，又会诱导在包内复制算法。
- **DSL/YAML**：判定规则包含流程编排、参数分组、采样策略（VL samples 翻转保护），配置表表达不了；一上表达式引擎就是劣化版编程语言，调试体验更差。
- **子进程**的真正设计点：插件**只写「规则与流程」，算力一律走宿主**。宿主把 L0/L1 实现暴露成能力网关端点，插件不自带算法——既防包内复制内置实现（§7 builtin_guard），又让算子升级只发生在宿主一侧。

### 5.2 进程模型

- `pack-host` 是**宿主内连接管理器**（`PackHost`）：不把插件代码加载进宿主进程，只管理子进程连接。
- **每 pack 一子进程**（PPID = 宿主），**懒启动**：首次调用（或设置页「测试」）才拉起。
- 装配：解释器三级解析（包内 venv → 内置 runtime → 裸 `python3`）+ 启动器 `<解释器> -m dampack_launcher <pack_dir>`；注入 `DAMPACK_*` 环境变量（网关地址/令牌/包根/工作目录/日志级别）。
- 生命周期：`health` 握手（2s）→ 失败重启 ×2 → 仍失败冷却 300s。
- **用完即析构**：超时 → 发 cancel 通知 → 宽限 2s（插件以 `-32004` 收尾则保留进程）→ **kill 整个进程组**（独立进程组是 spawn 时设的，避免 venv 孙进程残留）；断链（stdout EOF）记一次崩溃 → 重启重试 → 耗尽 → 冷却 + `Err(Crash)`。
- Windows：子进程带 `CREATE_NO_WINDOW`（python.exe / nvidia-smi 都是 console 子系统程序，GUI 宿主不设标志会闪黑框）。

### 5.3 能力网关：15 端点与 403 分界

插件唯一的算力入口。宿主拉起子进程前启动网关：`127.0.0.1:<随机端口>` + **一次性 Bearer 令牌**（经环境变量注入，不落盘）。端点 → 能力白名单机器可读于 `pack-host::gateway::ENDPOINTS`（15 项，与协议文档逐字一致）：

- `io/read`（读图 + 尺寸/哈希/白边测量）
- `detect/u2net`、`detect/bbox`、`detect/ocr`、`detect/white_stats`
- `op/crop`、`op/resize`、`op/letterbox`、`op/upscale`、`op/encode`（几何族 `op.geometric`）
- `op/watermark`（`op.overlay`）、`op/replace_bg`、`op/inpaint`（生成族 `op.generative`）
- `vl/judge`（VL 判定通道）、`report/emit`（进程内按 job 归账）

三条硬口径：

1. **能力缺失 → 403 `capability_missing`**：manifest 未声明该能力、模型未安装、本地推理没编进二进制（容器形态调本地算子），一律 403；插件据此回 `-32001`。**绝不静默降级成假结果**。
2. **写产物必须落在网关写根之下**（默认 = 该 job 的 `work_dir`）：越界 400，写前自动建父目录。
3. **VL 不可判不许冒充**：VL 后端报错 → `200 {undetermined: true, reason}`，由插件落 `undetermined` 行；只有「根本没配 VL 通道」才 403。VL 通道由 L3 装配注入（`GatewayConfig::with_vl`）——`pack-host` 与 `svc-pipeline` 同层禁互引，靠端口反转跨层。

### 5.4 结果行契约

每个资产一行结果：**status = `ok` / `failed` / `skipped`，verdict = `pass` / `fail` / `undetermined`**。

- `ok` + `pass/fail`：插件给出确定结论。
- `ok` + `undetermined`：插件自己声明不可判（VL 报 `undetermined:true`、规则参数缺置信度等）。
- **超时 / 崩溃 / 加载失败 → 一律 `undetermined` + `note`**（`plugin_timeout` / `plugin_crash` / 加载错误尾部 40 行 stderr）——宿主侧铁律「**不可判定绝不冒充**」：宁可留白给人审，不造数。
- 规则级失败不拖垮整图：单规则异常落 `undetermined + reason=rule_error`，其余规则照常。

### 5.5 workdir 边界与默认禁网

- 插件进程**默认禁网**：不直接联网、不自己实现算法（协议铁律）；一切算力与 IO 经网关（路径校验 + 能力白名单 + 令牌）。拿到令牌 ≠ 拿到宿主：令牌一次性、仅本机回环、随进程销毁。
- 插件文件写入仅限其 `work_dir`（网关写根校验，越界 400）；读原图走 `io/read`，不直接持有库内路径权限之外的东西。

### 5.6 venv 生命周期

- **装包时**用内置 runtime 给包建独立 venv（`<pack>/venv`）——首判不该为建环境卡几十秒；「哪个解释器建的」是一次性事实，装时定格。
- 建失败**不阻塞安装**：插件仍可用内置 runtime 跑（venv 只承载包内私有依赖）。
- **运行时只探测不创建**：包内 venv 解释器 → 内置 runtime（`config.runtime_python`）→ 裸 `python3`，探测逻辑只在 `pack_host::process` 一处。

---

## 6. 判定可审计

- **确定性开关**：包级 `deterministic` 由 manifest 声明。确定性包 → confidence 恒 1.0；非确定性包 → 插件给的 `score` 夹到 [0,1]，没给就 0.0——置信度语义由包自己负责，宿主不伪造。
- **VL 温度写死**：判定通道解码参数固定 `temperature=0, top_p=1`（同输入同结论）；采样上限 `samples ≤ 3`（翻转保护只需少数采样，不许包随意烧钱）。
- **结果身份 `packId@version#hash12`**：每行结论带包身份三元组，hash = `pluginHash`（manifest 原文 sha256）前 12 位——结论可精确追溯到产生它的那一份包内容；`api_version` 严格相等匹配，包内容一变 hash 即变。
- **判定数据下沉 job 账本**：结论行在 `compliance_runs`（job 级流水），人工改判落 `human_review`（append-only：内容版本变更自动失效、重跑仍生效、幂等）；资产 API 不再携带判定字段（显式 400），杜绝「资产快照 vs 判定流水」两套真相。

---

## 7. 取舍清单

| 备选 | 裁决 | 理由 |
|---|---|---|
| YAML / DSL 规则 | **否** | 配置表达力不够：判定含流程编排/参数分组/采样策略；表达式引擎 = 劣化编程语言，调试更差 |
| WASM 沙箱 | **否** | 跑不了真实算子（Python 生态 + 模型前后处理）；宿主已有 L0/L1 实现，WASM 只会诱导包内复制算法 |
| LanceDB | **否** | 依赖成本（protoc/arrow 链路）；sqlite-vec 是单 C 文件、rusqlite 原生静态编入。spike 实测 100K×512 KNN k=10/100 = **41/45ms**（<1s 验收线的 20 倍余量）。代价：JOIN 后过滤是 lossy post-filter → 过滤语义由 `engine-search` 预取候选 id |
| 插件自带模型/算法 | **否** | 内置核心功能不得被包成插件（`builtin_guard` 测试 R1–R5 + P1/P2 钉死）：打标/查重/EXIF/OCR/向量索引/语义检索不装不卸——否则用户「先装插件才能打标」，卸载还让既有结论与索引漂移。**模型与 provider 可换，那是配置不是插件**。例外口径：`engine-ocr` 是宿主网关 provider，必须留在 pack-host 依赖闭包内（包调网关，宿主做 OCR） |

---

## 8. 命名与历史

- 应用标识 `com.soujpg.dam.desktop`（`app-tauri/tauri.conf.json`）：项目孵化期的反向域名，沿用为**稳定标识**——它只是 identifier，**不代表任何现存商业关系**。
- 为什么首版保留：桌面 `{appData}` 目录与已装包/库数据布局由 identifier 派生，改名即要求一次 appData 迁移（数据搬迁 + 回滚路径）。首版保持不动；迁移留到有真实需求时作为独立变更做。

---

*相关文档：`README.md`（定位与上手）· `docs/industry-pack-protocol.md`（协议字段/方法权威）*
