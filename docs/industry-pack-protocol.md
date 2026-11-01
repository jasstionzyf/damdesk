# DAMDesk 处理插件协议 v2.0

> 状态：**v2.0 规范**（2026-11-01 契约裁决 v3 终稿落地；首版冻结 2026-10-20）。
> 协议名与语义改动必须升 `api_version`，并在文末「变更记录」留痕；宿主按 `api_version` 严格相等匹配，
> 不兼容即拒装（不做多版本兼容层，也不做 v1.x 协议 shim）。
>
> 命名主轴（2026-11-01 裁决，修订 2026-10-22 IP-K16 的行业中心命名口径）：
> **插件 = 通用批处理能力单元——输入一批图，输出每图一行结果（处理产物和/或判定结论）**。
> 行业是插件可带的属性（domain），合规是能力的一种；类型名 = 插件 / 插件包（`.dampack`），
> 「行业包」降为俗语（绑行业 domain 的插件）。v1.2→v2.0 术语映射见 §4.0.2 与 §11。
>
> 配套可执行规格：`tools/pack-conformance/`（12 条断言 + 假网关 + 参考实现 + 4 个违规变体）。
> 架构裁决与为什么是子进程代码插件（而不是 YAML/DSL/wasm）：`damdesk-repo`
> references/industry-pack-plugin-arch.md。本文只讲**协议面**，不讲产品取舍。

---

## 0. 一句话

处理插件 = **独立子进程**，用 **stdio 上的行式 JSON-RPC 2.0** 跟宿主说话；
宿主只提供**能力网关**（几何 / 分割 / OCR / VL / IO / 报告）。
母版：**插件输入一批图，输出每图一行结果（处理产物和/或判定结论）**——
判定、修复、处理的步数、顺序、分支全在插件代码里，宿主不解释流程；
v2.0 只有一个执行动词 `run`（§3.3）。

> 边界（N11 架构守卫，可执行断言：`crates/pack-host/tests/builtin_guard.rs` + `scripts/builtin_guard.sh`）：
> 本协议只管**可安装插件**。**内置核心功能**——批量打标、查重去重、EXIF / OCR、向量索引与语义检索——
> 是「所有用户都要、实现唯一、只需开关/参数/模型选择」的一档，**永远保持内置**：
> 不因为「能包成插件」而重包，也不出现在 manifest / `.dampack` / 插件列表里。
> 换模型 / 换 provider 是**配置**，不是插件。插件能拿到的 OCR 等算力只经能力网关（宿主侧实现）。

---

## 1. 传输层

> 实现侧说明：谁启动谁、父子进程关系、解释器选择、能力网关回环、故障冷却等实现细节，见
> `docs/rust-python-plugin-process-model.md`（本文只管契约，不管实现）。

| 项 | 规定 |
|---|---|
| 进程 | 宿主拉起 `manifest.entrypoint` 指定的子进程，cwd = 包根目录 |
| 协议方向 | 宿主 → 插件：JSON-RPC 2.0 **请求** / **通知**；插件 → 宿主：**应答** / **通知** |
| 分帧 | 每行一个 JSON 对象，`\n` 分隔（不允许折行、不允许裸数组） |
| 编码 | UTF-8，序列化 `ensure_ascii=False`；Windows 侧写入显式 `newline="\n"` |
| stdout | **只准出现协议帧**。日志、进度、异常栈一律走 **stderr** |
| flush | 每帧写出后立即 flush（宿主按行读取，不 flush 等于不回包） |
| 并发 | 插件**必须**能并发：`run` 执行期间仍要能处理 `cancel`（建议每请求一线程） |
| 幂等 | 同一 `id` 只回一帧；通知（无 `id`）不得回应答 |

### 1.1 宿主注入的环境变量

| 变量 | 含义 |
|---|---|
| `DAMPACK_GATEWAY_URL` | 能力网关地址，形如 `http://127.0.0.1:51423`（**每次启动随机端口**） |
| `DAMPACK_GATEWAY_TOKEN` | 一次性 Bearer 令牌，随进程生命周期有效，进程退出即废 |
| `DAMPACK_PACK_DIR` | 包根目录绝对路径（读 `resources/` 用） |
| `DAMPACK_WORK_DIR` | 本包可写的临时工作目录（产物落盘默认在此之下） |
| `DAMPACK_LOG_LEVEL` | `debug` / `info` / `warn` / `error`（只影响 stderr 详细度） |

插件**不得**从别处取地址/令牌（不读宿主配置、不扫端口、不联网）。

---

## 2. 生命周期与超时铁律

```
解压/验签 → 懒启动子进程（首个任务或设置页点"测试"时）
   → health 握手（2s 上限）→ 派发任务
   → 崩溃：自动重启 ×2
   → 仍失败：该包进冷却（cooldown），宿主不退出
```

### 2.1 超时（硬铁律，宿主实现，插件必须配合）

```
请求发出 → 等 manifest.timeout_secs（run）
  → 超时：发 cancel 通知（`{"jsonrpc":"2.0","method":"cancel","params":{"job_id":...}}`，无 id）
  → 宽限 2s：等待插件以 -32004 结束在飞请求
  → 仍未收尾：kill 进程
  → 该 job 的结果一律记 `undetermined`，note = `plugin_timeout`
```

**超时绝不落 `pass`，也绝不落 `fail`** —— 无法判定就是无法判定（见 §7 铁律 1）。
崩溃路径同构：重启两次仍失败 → 结果记 `undetermined`，note = `plugin_crash`。

### 2.2 取消语义

- `cancel` 必须**立刻**置位（宿主不等 `run` 返回才发）；
- `run` 在工作循环里检查取消位，命中即回错误 `-32004`；
- 取消后**不得**再为该 job 发 `report/emit`、不得返回结果行（rows / verdicts）；
- 已落盘的产物保留（宿主管清理，插件不删）。

---

## 3. 方法契约

五个方法全部必实现（缺一即 conformance 不过）：`pack.info` / `health` / `run` / `report` / `cancel`
（v1.x 的 `judge` / `repair` 双执行动词已废止，合并为单方法 `run`）。

### 3.1 `pack.info`

```jsonc
// 请求
{"jsonrpc":"2.0","id":1,"method":"pack.info","params":{}}
// 应答
{"jsonrpc":"2.0","id":1,"result":{
  "id":"com.soujpg.pack.ecommerce",
  "name":"电商主图合规",
  "version":"1.2.0",
  "api_version":"2.0",
  "rules":[{"id":"A2_min_short_side","name":"主图短边 ≥1600","deterministic":true}],
  "capabilities":["io.read","detect.u2net","op.geometric","vl.judge"],
  "deterministic":false,
  "timeout_secs":60
}}
```

`id` / `version` / `api_version` / `capabilities` / `deterministic` / `timeout_secs`
**必须与 manifest 完全一致**（宿主以 manifest 为准，不一致即判包损坏）。

### 3.2 `health`

```jsonc
{"jsonrpc":"2.0","id":2,"method":"health","params":{}}
→ {"jsonrpc":"2.0","id":2,"result":{"status":"ok","pid":12345,"api_version":"2.0"}}
```
必须在 **2s** 内应答。`status` 非 `"ok"` 视为握手失败（进冷却）。

### 3.3 `run`（v2.0 唯一执行动词）

> **v1.x 历史方法名**：`judge`（判定）/ `repair`（修复）——v2.0 废止双方法，合并为本方法
> （repair 本质 = 处理，不是与执行并列的动词）。

```jsonc
// 请求：params = 输入集 + options
{"jsonrpc":"2.0","id":3,"method":"run","params":{
  "job_id":"job-7f3a",
  "asset":{"id":"asset-42","path":"/data/lib/ab/cd.jpg","mime":"image/jpeg",
           "width":2000,"height":2000},
  "rules":["A2_min_short_side","A4_subject_ratio"],   // 判定类：省略 = 全部规则
  "options":{}                                        // 包自定义，宿主不解释
}}
// 应答（rows 为推荐形态，全部应答形态见 §4.4）——行按 manifest.outputs 出：
{"jsonrpc":"2.0","id":3,"result":{"rows":[
  {"assetId":"asset-42","ruleId":"A2_min_short_side","result":"ok",
   "verdict":{"status":"fail","confidence":0.31,"reasonCode":"A2_min_short_side"}}
]}}
```

执行语义由 `outputs` 声明决定，方法只有一个：

| outputs | 一次 `run` 的行为 | 响应行 |
|---|---|---|
| `["verdict"]` | 只判定：必须**覆盖请求的每一条 ruleId**（无法判定就用 `undetermined`，不许少返回） | 判定行（`verdict` 必填） |
| `["artifact"]` | 只处理：**产物必须是宿主可读的磁盘路径**（不许回 base64、不许只在内存） | 处理行（`artifacts`） |
| `["verdict","artifact"]` | 判定+修复一条龙：修复后必须按规则复判（不是「改完就算过」）；每条 `artifact.rule_id` 必须有对应 verdict 且 `fixed=true` | 行带 `verdict` + `artifacts`/`fixed` |

- `fix_policy` 在 **options** 里门控只判不修（如 `judge_only` → 回 `result:"skipped"` + `note:"fix_policy=judge_only"`）；方法层不再有第二个执行动词；
- **先判后修（人工复核后再修）= 两次 `run`**（参数不同），各自独立 job 留痕（铁律 4：一次执行 = 一个 job，manifest 记动作+参数可复现）；
- 单条规则抛业务异常不等于整包失败：应落 `undetermined` 并写 `note`；
- 参数缺失/类型错 → `-32003`；资产读不出来 → `-32002`。

### 3.4 `report`

```jsonc
{"jsonrpc":"2.0","id":5,"method":"report","params":{"job_id":"job-7f3a"}}
→ {"jsonrpc":"2.0","id":5,"result":{
    "job_id":"job-7f3a","rows":2,          // v1.x 计数字段 judged → v2.0 rows
    "assets":["asset-42"],
    "entries":[{"assetId":"asset-42","verdicts":[Verdict...]}]
  }}
```

按 `job_id` 归账：A 任务的报告里**不许**出现 B 任务的资产（宿主据此出交付报告）。

### 3.5 `cancel`

```jsonc
{"jsonrpc":"2.0","id":6,"method":"cancel","params":{"job_id":"job-7f3a"}}
→ {"jsonrpc":"2.0","id":6,"result":{"canceled":true,"job_id":"job-7f3a"}}
```
幂等：重复取消同样回 `true`；对不存在的 job 也回 `true`。

### 3.6 插件 → 宿主的通知（可选，宿主忽略但会记日志）

```jsonc
{"jsonrpc":"2.0","method":"progress","params":{"job_id":"job-7f3a","step":"letterbox","done":2,"total":5}}
{"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"occlusion detected"}}
```

---

## 4. 数据结构

### 4.0 `manifest.json`（唯一配置面，宿主不读包内其他配置）

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | ✔ | 包唯一标识，反向域名式 |
| `name` | string \| object | ✔ | 展示名（可中文）。**i18n-F**：字符串 = 全语言通用（唯一向后兼容形态）；对象 `{"zh-Hans": "…", "en-US": "…"}` = 按语言给值，至少一条非空文案，见 §4.0.1 |
| `version` | string | ✔ | 语义化版本；发布后不得复用 |
| `api_version` | string | ✔ | 恒为 `"2.0"`（当前值，宿主严格相等匹配） |
| `entrypoint` | `{cmd,args[]}` | ✔ | 相对路径按包根解析；`python3` 由宿主替换为内置运行时 |
| `capabilities[]` | string[] | ✔ | 能力白名单，见 §6 |
| `deterministic` | bool | ✔ | 全规则确定性可复现才为 true |
| `timeout_secs` | int | ✔ | 1..3600，单图判定预算 |
| `outputs[]` | string[] | ✔ | 输出形态闭集 `verdict`（出判定结论）/ `artifact`（出处理产物）（§4.4）。**新插件必须显式声明、无缺省**；空数组、重复、未知值、大小写错 → **拒装**。出现旧字段 `kinds` → **拒装**，报错必须含映射指路：`kinds 已更名 outputs：judge→verdict / process→artifact`（§4.0.2） |
| `resources[]` | string[] | ✖ | 只读资源相对路径（清单，装机时校验存在） |
| `params` | string | ✖ | **K07-I（v1.1 内追加的可选键，不升 `api_version`）**：参数 schema 相对路径（如 `resources/params.schema.json`）。声明了 → 装机期强制校验（文件必须在归档里 + 能被宿主解析校验通过，否则拒装 `pack_params_missing` / `pack_params_schema`），`/run` 按它合并/校验 `options`；不声明 = 老包，`options` 原样透传（v1.1 老行为）。schema 格式与控件映射见 `damdesk-product` references/plugin-param-schema.md |
| `short_name` | string \| object | ✖ | **DM-B1（v1.1 内追加的可选展示键，不升 `api_version`）**：菜单/导航/卸载确认这类窄位置的**用户面短名**（§3 功能名，如「合规检查」）。缺省 = 回落 `name`。`GET /api/compliance/packs` 的 `plugins[].name` 就是它 —— 界面不得再维护 id → 名字 的映射表（KI-36） |
| `summary` | string \| object | ✖ | **DM-B1（同上）**：一句话「做什么 + 有什么用」（如「检查图片能不能直接用在电商平台（淘宝 / Amazon）上」）。缺省 = 不显示描述（不编造）。用户面文案禁止出现内部词（插件/规格/manifest/job/verdict/行业包） |
| `permissions` | `{read:bool, net:bool}` | ✖ | 默认 `{read:true, net:false}`；`net` 打开也必须经宿主代理 |
| `signature` | string\|null | ✖ | ed25519 签名串（`<appdata>/dam/packs` 安装时校验；开发期可为 null） |

`validate_manifest()`（`tools/pack-conformance/protocol.py`）是这台字段表的可执行版本。

#### 4.0.1 可选多语言字段（i18n-F，不升 `api_version`）

**哪些字段可本地化**：manifest 的 `name` / `short_name` / `summary`、rules.json 的 `rules[].name`、
`result_columns` 的 `label`（§4.5）—— 都是**用户面展示文案**。规则 `id`、`detail_columns`（规则 id）、
`report.summary`（档位词）是机器键，**不做**本地化。

**两种形态**：
- 字符串 `"电商主图合规"` = 全语言通用（唯一向后兼容形态；第三方包不提供英文也不报错）；
- 对象 `{"zh-Hans": "电商主图合规", "en-US": "E-commerce Product Compliance"}` = 按语言给值。
  键是 locale 标签（宿主只特殊对待 `zh-Hans`，其余原样精确匹配），值必须是非空字符串；
  `name` 至少给一条非空文案，否则拒装。

**宿主取值口径**（`core_config::packs::LocalizedText::pick`，与 conformance `pick_i18n` 同链）：
**请求 locale → `zh-Hans` → 任意一条（键序稳定）→ 空串**。回退发生时宿主 `eprintln` 提醒一次，
**不拒装不崩** —— 只给了中文的包在英文界面回退中文，这是可接受边界（翻译说明见包 README）。

**请求 locale 的来源**：HTTP `Accept-Language` 首个标签（`zh*` → `zh-Hans`、`en*` → `en-US`，
其余/缺失 → `zh-Hans`），前端 `api.ts` 统一注入。

**下发形态**：`GET /api/compliance/packs` 的 `plugins[]` 同时给取好的 `name`/`summary`（按请求语言）
与原样对象 `nameI18n`/`summaryI18n`；判定行给 `ruleName` + `ruleNameI18n`；结果列给 `label` +
`labelI18n`（字符串形态时 i18n 字段为 null）。前端切换语言用 i18n 对象即时重渲染，不需要重拉接口。
`GET /api/compliance/packs/{id}` 的 `manifest` 字段是 manifest 原样结构，对象形态原样透传。

#### 4.0.2 历史字段说明（v1.x → v2.0 映射）

- `kinds[]`（v1.1 引入，**v2.0 废止**）：值映射 `judge` → `verdict`、`process` → `artifact`；
  缺省 `["judge"]` 同时废止（v2.0 必须显式声明 `outputs`、无缺省）。
  新装出现 `kinds` → 拒装，报错含映射指路：`kinds 已更名 outputs：judge→verdict / process→artifact`。
- RPC 执行方法 `judge` / `repair`（v1.x）→ 合并为单方法 `run`（§3.3）。
- `report` 应答计数字段 `judged`（v1.x）→ `rows`。

### 4.1 规则与 PackInfo

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 包唯一标识，反向域名式（`com.soujpg.pack.ecommerce`） |
| `version` | string | 语义化版本；同 `id` 多版本可并存（回滚=切指针） |
| `api_version` | string | 协议版本，当前恒为 `"2.0"` |
| `rules[]` | `{id, name, deterministic}` | 规则清单；`deterministic=false` 只出建议不参与硬判定。**i18n-F**：`name` 可本地化（§4.0.1，字符串或 `{"locale": 文案}` 对象） |
| `capabilities[]` | string[] | 能力白名单（§6），未声明的端点调用一律 403 |
| `deterministic` | bool | 包级：是否全规则确定性可复现（含 VL 的包必须 false） |
| `timeout_secs` | int 1..3600 | 单图判定预算 |

### 4.2 Verdict（七个字段，缺一即协议违例）

```jsonc
{
  "ruleId":"A4_subject_ratio",
  "status":"fail",              // pass | fail | undetermined | skipped
  "score":0.31,                 // 数字或 null（无分数语义时用 null，别填 0 冒充）
  "label":"subject_ratio",      // 稳定的机器可读标签（报告列、筛选器用）
  "evidence":{"fg_ratio":0.31,"threshold":0.55,"mask_path":"..."},  // 对象，可为 {}
  "fixed":false,                // 本轮是否已修复（run 修复语义里才可能 true）
  "note":"below_threshold"      // 字符串或 null；不可判定时必须写原因
}
```

`status` 语义闭集：

| status | 含义 | 报告口径 |
|---|---|---|
| `pass` | 合格 | 绿色 |
| `fail` | 不合格（有依据） | 红色，必须给 `evidence` |
| `undetermined` | **无法判定**（VL 不可用/模型缺失/解析失败/超时/崩溃） | 灰色"待人工"，**不计入合格也不计入不合格** |
| `skipped` | 未适用（规则被跳过、只判不修、无该规则） | 灰色，`note` 说明原因 |

### 4.3 Artifact

`{rule_id, path, width, height, kind}`；`kind` 为 MIME（`image/png` / `image/webp`）。
`path` 必须在 `options.workdir` 或 `DAMPACK_WORK_DIR` 之下。

### 4.4 结果行契约（N5a，冻结；`result` 必填、`verdict` 只属判定类）

**执行结果 ≠ 判定结论**。判定类插件给审核结论；处理类插件（resize / 转格式 / 压缩 /
水印 / 抠图换背景 / 超分…）只有执行结果与产物，**没有审核状态**，也**不得**填 `verdict`。

```jsonc
{
  "assetId": 12345,                 // 必填：宿主用它把行贴回资产（数字/字符串都收，落地统一成字符串）
  "ruleId": "A4_subject_ratio",     // 判定类按规则出行时必带（行键 = assetId + ruleId）
  "result": "ok",                   // 必填：ok | failed | skipped —— 这一行「跑成了没有」
  "verdict": {                      // 可选块；只有 outputs 含 verdict 才有（且 result=ok 时必填）
    "status": "fail",               // pass | fail | undetermined（**没有 skipped**）
    "confidence": 0.31,             // 0..1 或省略
    "reasonCode": "A4_subject_ratio"// 机读原因码（自定义语义只能进这里 / fields / evidence）
  },
  "error": {"code": "asset_unreadable", "message": "解码失败"},  // result=failed 必填
  "skipReason": "原图缺失",          // result=skipped 必填
  "artifacts": [                    // 产物只准两种去处，不许自建目录树
    {"kind": "working_version", "assetId": "12345", "versionId": "v3"},
    {"kind": "export_file", "path": "exports/a.png", "sha256": "…"}
  ],
  "evidence": {},                   // 自由结构（bbox/OCR/模型回复/耗时/参数）：只展示不参与逻辑
  "fields": {}                      // 行业自定义列，宿主当不透明字典渲染（列声明见 §4.5）
}
```

应答形态（三者宿主都收，`rows[]` 是新契约的推荐形态）：

| 形态 | 结构 | 用在 |
|---|---|---|
| 行数组 | `{"rows":[Row...]}` | 处理类按资产出行、判定类按规则出行 |
| 单行 | `{"row":Row}` | 一次调一行的包 |
| 旧判定 | `{"asset_id":"…","verdicts":[Verdict...]}` | **v1.x 历史应答，v2.0 新包不得使用**（宿主**逐规则 1:1** 归一化成行，与 `rule_verdict` 表既有行对应） |

硬规则（宿主实现 = `crates/pack-host/src/result.rs`；违例码即 `evidence.reason` 词表）：

| 规则 | 违例码 |
|---|---|
| `result` 缺失 / 不在三值内 | `row_result_missing` / `row_result_invalid` |
| `result=failed` 没带 `error{code,message}` | `row_error_missing` |
| `result=skipped` 没带 `skipReason` | `row_skip_reason_missing` |
| **处理类插件回了 `verdict`** → 整批拒收（不折算、不静默丢弃） | `row_verdict_not_allowed` |
| 判定行 `result=ok` 却没给 `verdict` | `row_verdict_missing` |
| `verdict.status` 越界（写 `skipped`、自造「待复核」…） | `row_verdict_status_invalid` |
| `confidence` 不在 0..1 | `row_verdict_confidence_invalid` |
| 产物不是 `working_version`/`export_file`（或缺必填键） | `row_artifact_invalid` |
| `evidence` / `fields` 不是对象 | `row_evidence_not_object` / `row_fields_not_object` |
| `result=ok` 带 `skipReason` 且 `artifacts` 为空 | **允许**（处理类的「合法跳过」，见口径 4） |

三条配套口径：
1. **宿主补 `assetId`**：包漏回 `assetId` 时宿主用「它自己请求的那张图」补齐（可推导字段不值得整批作废），
   但 conformance 侧**不补**（包必须自己回）。
2. **混合包**（`outputs=["verdict","artifact"]`）：带 `ruleId` 的行是判定行（`verdict` 必填），
   不带 `ruleId` 的行是资产级处理行（`verdict` 可选）。
3. **旧 `verdicts[]` 归一化（v1.x 历史应答）**：`pass`/`fail`/`undetermined` → `result=ok` + `verdict{status, confidence=score 夹到 0..1}`；
   `skipped` → `result=skipped` + `skipReason`（`note` 原文）；`label`/`note` 原文进 `evidence`（人读文本不塞进机读 `reasonCode`）。
4. **处理类的「合法跳过」**（历史修复）：`result=ok` + `skipReason`（如 `already_white`）+
   **空 `artifacts`**，语义是「这一行跑完了，结论是不需要新版本」。与 `skipped` 的区别是**做了没做**：
   `skipped` = 这一行没跑（必须带原因，不产出）；`ok+skipReason` = 判定已完成、结论是 no-op（同样不产出）。
   下游据此**不落工作版本**；不许拿它掩盖失败（失败一律 `result=failed` + `error{code,message}`），
   也不许拿它当「算了不做了」的偷懒出口——跳过条件必须是可实测、可复算的（例：抠图包的输入边框带实测 ≤3）。

### 4.5 `result_columns`（列声明，渲染侧；N5b 落地）

插件在 `resources/` 声明 `result_columns: [{key,label,type(number|text|enum|bool),unit?,order}]`，
宿主只按 schema 画表格/排序/筛选/导出；`outputs` 含 `verdict` 才追加审核列（状态 / 原因 / 置信度）与复核动作。
未声明 → 默认列（资产 / 执行结果 / 产物 / 备注），不许报错。
**i18n-F**：`label` 可本地化（§4.0.1）——字符串或 `{"locale": 文案}` 对象；宿主按请求语言取值下发
`label` + 原样 `labelI18n`。`key`/`type`/`unit` 是机器键，不本地化。

---

## 5. 错误码

| code | 名称 | 何时用 |
|---|---|---|
| `-32001` | 能力缺失 | 网关 403 `capability_missing`、包未声明该能力、模型未安装 |
| `-32002` | 资产不可读 | 网关 404 `asset_unreadable`、解码失败、权限不足 |
| `-32003` | 参数非法 | 缺 `asset.path`、类型错、`box` 越界 |
| `-32004` | 已被取消 | 命中取消位（`cancel` 之后） |
| `-32005` | 插件侧超时 | 插件自己的内部截止（宿主超时另有口径，见 §2.1） |
| `-32000` | 其他 | 兜底，**必须**带可读 `message`；不得用来代替上面任何一类 |
| `-32601` | 方法不存在 | JSON-RPC 标准码：未实现的方法 |
| `-32700` | 解析错误 | 收到的行不是合法 JSON |

**禁自造码**：不在这张表里的 code 一律视为协议违例（宿主按 `-32000` 兜底并记违例计数）。
错误帧必须是 `{"jsonrpc","id","error":{"code","message"}}`，**带 `error` 就一定不带 `result`**。

---

## 6. 能力网关（插件唯一的算力入口）

- 地址 `127.0.0.1:<随机端口>`，`Authorization: Bearer <DAMPACK_GATEWAY_TOKEN>`；
- 每次启动生成令牌，进程退出即废；
- 按 `manifest.capabilities` 白名单放行，未声明 → `403 {"error":"capability_missing","capability":"..."}`；
- 请求/应答全部 `application/json`；`path` 一律磁盘绝对路径（插件不传 base64 大图）。

| # | 端点 | 能力 | 请求 | 应答 |
|---|---|---|---|---|
| 1 | `POST /io/read` | `io.read` | `{path}` | `{path,size,sha256,mime,width,height}`；不存在 → 404 `asset_unreadable` |
| 2 | `POST /detect/u2net` | `detect.u2net` | `{path}` | `{path,fg_ratio,mask_path,width,height,bbox?,dominant_bbox?,instances}` |
| 3 | `POST /detect/bbox` | `detect.u2net` | `{path,target_ratio?,min_instance_ratio?}` | `{path,width,height,fg_ratio,bbox,dominant_bbox,instances,min_instance_ratio,window?,grid,engine}` |
| 4 | `POST /detect/ocr` | `detect.ocr` | `{path,lang?}` | `{path,lines:[{text,box:[x1,y1,x2,y2],score}],engine}` |
| 5 | `POST /detect/white_stats` | `detect.white` | `{path,band?,max_dev?}` | `{path,width,height,mean_dev,worst_dev,band,edge_pixels,threshold,pass,engine}` |
| 6 | `POST /op/crop` | `op.geometric` | `{path,box:[x1,y1,x2,y2],out}` | `{path,width,height,op}` |
| 7 | `POST /op/letterbox` | `op.geometric` | `{path,target_ratio,background?,out}` | `{path,width,height,op,target_ratio}` |
| 7b | `POST /op/resize` | `op.geometric` | `{path,max_side,out}` | `{path,width,height,source_width,source_height,scaled,max_side,op}`（**只缩不放**；`max_side` ∈ 16..=8192；`out` 不许等于 `path`） |
| 6 | `POST /op/upscale` | `op.geometric` | `{path,scale,out}` | `{path,width,height,op}` |
| 8 | `POST /op/watermark` | `op.overlay` | `{path,text,out,position?,opacity?,font_size?,margin?,color?}` | `{path,width,height,op,coverage}`（尺寸不变；`opacity` = 1..=100 整数百分比；`position` ∈ top_left/top_right/center/bottom_left/bottom_right） |
| 9 | `POST /op/replace_bg` | `op.generative` | `{path,color,out}` | `{path,width,height,op}` |
| 10 | `POST /op/inpaint` | `op.generative` | `{path,mask,out}` | `{path,width,height,op}` |
| 11 | `POST /vl/judge` | `vl.judge` | `{path,prompt,max_tokens,samples?}` | `{path,undetermined,raw,rules?:[{status,label,score}],samples?,raws?}` |
| 12 | `POST /report/emit` | `report.emit` | `{job_id,entry}` | `{ok,job_id}` |
| 13 | `POST /op/encode` | `op.geometric` | `{path,out,format?,quality?,background?,auto_orient?}` | `{path,format,width,height,bytes,bytes_in,source_alpha,alpha,quality_applied,quality,orientation,op}` |

约定：

- `out` 必须落在 `DAMPACK_WORK_DIR`（或本 job 的 workdir）之下，越界 → 400；
- `/io/read` 的 `measure:["white"]` 是**可选**扩展（K07-FIX-D）：带它才多解一次图并回
  边缘白度统计（`white_dev` = 外圈 `band` 像素带内「与纯白的最大通道偏差」均值，
  与旧引擎 classic `white_edge` 同一份实现，`band` 默认 4）。不带 = 应答与 v1.0 逐字一致；
  包用它做**确定性**背景白度判定，不必为白底额外花一次 VL 往返；
- `/vl/judge` 的 `max_tokens` 上限 4096；应答 `undetermined:true` 时插件**必须**落 `undetermined`；
- `/vl/judge` 的 `samples`（**1.2 增量**，`1..3`，缺省 1）是「同一 (图片内容, prompt, 模型)
  重复采样几次」：`>1` 时应答回 `raws:[{undetermined,raw,reason?,rules?}]`，顶层 `raw`/`rules`
  仍是**第一份**（1.1 的读法不变）。用处是**翻转保护**：语言模型的单发结论会抖，
  包可以要求 3 份采样一致才采纳，不一致就落 `undetermined`（K07-FIX-H）。
  宿主侧第 2..N 份**绕过结果缓存**取新样本 —— 否则「3 次一致」是缓存自证，等于没测。
  1.1 宿主收到这个字段直接忽略（只回单份 `raw`），包必须能按单份降级；
- VL 的**解码参数由宿主固定**：`temperature=0` + `top_p=1`（`compliance_vl::chat_inner` 写死，
  `/vl/judge` 的请求体**不收**这两个字段）—— 插件要求在 evidence 里如实记录这两项，
  否则同一 (内容, prompt, 模型) 换个温度就换了结论，复现性无从谈起（K07-FIX-I）；
- 网关错误映射：`403 → -32001`、`404/410 → -32002`、`400 → -32003`、其他 → `-32000`；
- **错误帧必须可定位**（v1.0 追加字段，向后兼容）：除 `error` / `detail` 外还回
  `op`（出错端点）与 `params`（实际请求参数，字符串超 240 字符截断）。
  插件侧把 `error`+`op`+`params`+`detail` 一起写进 `-32003` 等错误文案 —— 不许只回
  「参数非法（400）invalid_params」这种无法定位的裸错误（K07 D7）。
- **`upscale` 的 `scale` 是总倍数，不是单趟倍数**：宿主内部按趟执行（每趟长边 ×≤8，
  末趟精确落到目标长边），应答 `passes` 报趟数、`steps` 报每趟的 `from_long/to_long/mode`、
  `scale_max` 报这张图的可用上限；`op` 为 `upscale.realesr-x4v3`（单趟 4× 且模型在盘）/
  `upscale.resample`（单趟重采样）/ `upscale.staged`（多趟）。
  硬门槛只有两条：目标像素 ≤ 1e8、目标长边 ≤ 20000（另有防呆总倍数 64）。
  超门槛 → `400`，`detail` 里给出**可用上限 + 建议参数**（`scale_max`、短边/长边可达值），
  插件据此自行调小 `scale`，不要靠猜。
- **`letterbox` 是几何修复的关键算子**：`crop` 只裁不补（白边/尺寸各自坏），
  `crop → letterbox 到目标占比 → upscale 到短边目标` 可闭式一次求解，不需要多轮迭代；
  **但补白是可选动作**：方形画布会放大「主体占比」的分母，宽主体图补成 1:1 会把占比压破下限
  （电商包 a002 实测 0.715 → 0.385）。协议不强制 1:1，插件在「补白会压破下限」时应保留裁剪窗比例
  （电商包做法：`plan.letterbox=false` + `trail` 落 `letterbox_skipped`，见该包 README §修复）；
- `/op/watermark`（1.1 追加，能力 `op.overlay`）是**叠加不是变形**：产物尺寸 = 源尺寸，
  只有水印字形覆盖的像素变化。字体**内嵌**（Noto Sans SC，OFL 1.1）+ 纯 Rust 光栅化，
  中英文同一路径、两平台同字节；`font_size` 像素字号、`opacity` 1..=100 整数百分比、
  `margin` 距最近边距像素（要求 `2×margin < 短边`）、`position` 五锚点。
  v1 是**单行**文本：含换行或字体缺字形（emoji 等）→ `400`，绝不静默跳字/静默空产物；
  应答带 `coverage`（字形包围盒面积 / 画布面积），插件写进 `evidence` 便于核对「确实出墨」。
  落位与混合口径与旧 C4 `watermark` 变换逐字相同（老规则迁插件后产物一致）；
- `/detect/bbox`（1.1）回**同一张掩码**的框：`bbox` = 全掩码外接框、`dominant_bbox` = 最大连通域
  （主主体）紧框，均为源图像素 `[x1,y1,x2,y2]`（左闭右开，与 `/op/crop` 同口径）；无前景时三者皆 `null`；
  `instances` = **全部商品实例**（掩码每个 4-连通域一个 `{bbox,area_ratio}`，按面积降序、最多 8 个），
  `area_ratio` 是该连通域前景占画面比例（与 `fg_ratio` 同网格口径，可加总核对）；只留占画面
  ≥ `min_instance_ratio`（默认 `0.01`）的连通域 —— 散点噪声不算商品。多商品图（一串胶囊、一排货架商品）
  里第二件商品是独立连通域，插件判「文字是不是商品自带」必须按实例集合算，只认主主体会把商品自带
  文字误判成主体外促销条（K07 f002 实测：3 行 OCR 合计 3.39% → 误判 fail）。老宿主没有该字段时插件
  退回单主体口径，行为不变；
  给 `target_ratio`（`0<r≤1`）时再回 `window:{box,ratio,target_ratio}` —— 主主体中心对齐、
  掩码占比最接近目标的窗，`ratio` 是窗内网格占比（预测占比用它算，与复检同网格口径）；
  同图连调 `/detect/u2net` 与 `/detect/bbox` 宿主只推理一次（宿主可缓存最近一次掩码）；
- `/detect/white_stats`（1.1）是**纯像素统计、不需要模型**：外圈 `band`（默认 4，钳到短边一半）
  像素带内每像素取 `255-min(R,G,B)`，`mean_dev` = 全带均值、`worst_dev` = 最差单像素，
  `pass = mean_dev ≤ max_dev`（默认 8）；中心主体不参与统计。与旧引擎 `white_edge` 检测器
  共用 `engine-classic` 同一份实现 → 逐值同口径、同图同参数同结果（`deterministic`）。

---

## 7. 硬铁律（一条不满足即判包不合格）

1. **不可判定绝不冒充**：VL 不可用、模型缺失、超时、崩溃 —— 一律 `undetermined` + `note`；
   不许按"大概率没事"落 `pass`，也不许落 `fail` 充数。
2. **判定确定性可复现**：`deterministic:true` 的包，同图同参数必须同结论（禁随机、禁时间相关）；
   含生成式/VL 的包必须标 `deterministic:false`，其结果只作建议。
3. **插件不直接联网**：算力只经网关；`permissions.net` 默认 false，开了也要走宿主代理。
4. **stdout 纯净**：一行日志混进协议流 = 宿主无法解析 = 包不可用。
5. **job 隔离**：结果与 `report/emit` 必须按 `job_id` 归账，不串。
6. **产物落盘可交付**：修复产物是磁盘路径，导出/审核/复检都基于它，不是内存字节。
7. **每条结果可追责**：宿主在结果行落 `packId@version + pluginHash + deterministic`，
   所以 manifest 的 `id`/`version` 一旦发布不得复用（改内容必须升 version）。
8. **日志不得拖垮业务**（N19 血案）：包内 logger **永不抛异常** —— stderr 写不进去就降级到文件；
   工作线程也**不许「没有应答就结束」**（`except BaseException` 兜住 `SystemExit` 这类静默退出）。
   真机踩过（v1.x judge 时期，规则同样适用于 v2.0 `run`）：Windows 上同一进程从第 2 次执行起 `sys.stderr.write` 抛 `OSError(22)`，
   日志异常穿透业务把工作线程打死，宿主只能等满预算报 `plugin_timeout`、包内零日志。
9. **应答不依赖文本层**：协议帧建议 `os.write` 直写 fd（短写续写、0 字节显式报错）；
   文本层与 fd 两路都失败才允许放弃，且必须留证。

---

## 8. 版本与兼容

- `api_version` 当前为 `"2.0"`；宿主**严格相等**匹配，`<2.0` 的包装不进 v2.0 宿主（反之亦然）。
  v2.0 相对 v1.x 是**契约改名**（`kinds`→`outputs`、`judge`/`repair`→单方法 `run`、`report` 计数
  `judged`→`rows`，见 §11），网关端点、错误码闭集、Verdict 与结果行数据结构语义不变。
- **兼容口径（一次迁移，不留长期双轨）**：官方包同批升 v2.0（manifest + run 实现）；新装只认 v2.0
  （出现 `kinds` 或 `api_version` < 2 → 拒装 + 报错指路映射：`kinds 已更名 outputs，值映射 judge→verdict / process→artifact；执行方法 judge/repair 已合并为 run`）；
  已装 <2.0 包标记待升级，重新导入即恢复；**不做协议层 shim**。
- `manifest.json` 是**唯一配置面**：宿主不读包内 YAML、不解释包内脚本。
- 包的 `capabilities` 只能缩小不能"自带实现"：声明了不代表一定有权限（部署方可只授权子集）。
- 已发布 version 的目录永不就地修改；升级=装新版本目录 + 切 `current` 指针，回滚=切回指针。
- **已装机器的升级姿势 = 新版本号 + activate**：内置包（`pack_builtin_protected`）不可卸载，
  同 `id@version` 换内容会被宿主拒装（`pack_version_conflict`，判等口径 = `manifest.json` 原文 sha256），
  所以「重导同号包」在已装机器上**永远装不进去**。出包侧的硬校验与台账见 `packs/README.md`
  「发布与版本号」+ `scripts/pack_release_guard.py`（内容变了必须升 `version` / `PACK_VERSION`）。

---

## 9. 包形态

```
<id>.dampack (zip)
├── manifest.json          # 唯一配置面
├── plugin/
│   └── __init__.py        # Python 形态：导出 PACK_INFO / HANDLERS；可直接当脚本跑
├── vendor/                # 可选：包内私有依赖（sys.path 里排在 host shared-site 之前）
├── resources/             # 只读资源（词表、模板）
└── README.md
```

Python 形态的三个必守（踩过的坑）：

```python
# 1) 先把 fd1 存下来，再把 fd1 接到 stderr —— 任何库里的 print 都不会污染协议
raw_out = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")
os.dup2(2, 1)
sys.stdout = sys.stderr

# 2) 逐帧序列化：utf-8 + ensure_ascii=False + 立即 flush
raw_out.write(json.dumps(frame, ensure_ascii=False) + "\n"); raw_out.flush()

# 3) 每请求一线程：run 执行期间必须还能处理 cancel
threading.Thread(target=handle, args=(req,), daemon=True).start()
```

Windows 额外：`os.add_dll_directory(os.path.join(pack_dir, "vendor"))`（有原生扩展时）。

---

## 10. conformance（任何语言的包都必须过）

```bash
# 验包
python3 tools/pack-conformance/run.py --plugin <包目录>          # 14/14 PASS，退出码 0
python3 tools/pack-conformance/run.py --plugin <包目录> --json-out /tmp/report.json

# 验套件自身（好包 14/12 全绿：字符串形态 good + 对象形态 good-i18n；四个违规变体必须命中各自的预期断言）
bash tools/pack-conformance/run_all.sh
```

假网关只回**固定桩数据**（按资产文件名约定，见 `tools/pack-conformance/gateway.py` 文件头），
**不验任何算法正确性**；它验的是"你怎么用网关"。算法质量由 K00 基线 / K07 等价门禁 / K11 一致性负责。

### 10.1 十四条断言

| # | 断言 | 不通过的典型原因 |
|---|---|---|
| 01 | manifest 合法 | 缺字段、`api_version` 不匹配、未知能力、args 指向不存在文件 |
| 02 | stdout 只走协议行 | 库/调试 `print`、启动 banner 打到 stdout |
| 03 | `pack.info` 与 manifest 一致 | id/version/capabilities 抄错 |
| 04 | `health` ≤2s 应答 | 握手挂死、阻塞在初始化 |
| 05 | 五个方法齐全 | 忘实现 `report` / `cancel` |
| 06 | Verdict 七字段 + status 闭集 + 结果行归一化（§4.4） | 少字段、status 用了自造值、ruleId 与请求不符、应答无法归一化成合法结果行 |
| 07 | VL 不可判 → `undetermined` | 把 `undetermined` 提升成 pass/fail |
| 08 | 能力白名单：未声明必须 `-32001` | 吞掉 403、绕过网关自己造结果 |
| 09 | 错误码用法 | 参数错用 `-32000`、自造 `-1`、未知方法不回 `-32601` |
| 10 | cancel：run 期间可取消且以 `-32004` 结束 | 单线程阻塞、取消后仍上报 |
| 11 | run 产物落盘且与 verdict 对账 | 回 base64、产物不存在、没标 `fixed` |
| 12 | job 归账隔离 | 报告/emit 串账、只用全局状态 |
| 13 | i18n 字段：字符串/对象两形态可解析（§4.0.1） | 展示文案写了空串/空对象/值非字符串 |
| 14 | i18n 取值回退：locale → zh-Hans → 任意 → 空串 | —（宿主取值口径的契约断言，含字符串形态回归） |

判定类断言（02/05/06/07/08/09/10/11/12）对 `outputs` 不含 `verdict` 的处理类包**如实跳过**
（输出标明本包为处理类、无判定面），不冒充通过。

> 套件同步：`tools/pack-conformance/` 与 pack_lint 随 v2.0 代码卡同批升级（用例改 run + outputs，
> 补「api_version<2 拒装」「kinds 拒装指路」两条；pack_lint 强制显式声明 outputs）；
> 升级落地前套件按 v1.x 词汇运行，语义对照本节执行。

### 10.2 套件自检夹具

| 夹具 | 违反什么 | 预期失败断言 |
|---|---|---|
| `fixtures/good` | 无（参考实现；字符串形态名） | — |
| `good-i18n`（`run_all.sh` 运行时从 good 派生到临时目录） | 无（manifest 名字/短名/描述全用对象形态） | — |
| `fixtures/bad-stdout-pollution` | banner/调试打到 fd1 | 02 |
| `fixtures/bad-missing-method` | 未实现 `report`/`cancel` | 05、10、12 |
| `fixtures/bad-error-code-misuse` | 参数错用 `-32000`、自造 `-1`、吞 403 | 08、09 |
| `fixtures/bad-no-health` | health 永不应答 | 04 |

`run_all.sh` 对这几组**逐条比对失败编号集合**：只要某变体的失败项多一条或少一条就算套件退化。

---

## 11. 变更记录

| 日期 | 版本 | 改动 |
|---|---|---|
| 2026-11-01 | 2.0 | **契约改名（2026-11-01 裁决 v3 终稿）**：manifest `kinds`→`outputs`（值 judge→verdict / process→artifact；新插件必须显式声明、无缺省；出现 kinds → 拒装 + 报错含映射指路）；RPC 执行动词 `judge`/`repair` 废止，合并为单方法 `run`（fix_policy 归 options 门控只判不修；先判后修 = 两次 run 各自 job 留痕）；`report` 计数 `judged`→`rows`。兼容口径：**一次迁移不留双轨**——官方包同批升 v2.0，新装只认 v2.0（出现 kinds 或 api_version<2 拒装指路），已装旧包重导入即恢复，**无协议 shim**。网关端点 / 错误码 / Verdict 与结果行数据结构语义不变 |
| 2026-10-20 | 1.0 | 首版冻结：六个方法、Verdict 七字段、8 个错误码、网关 10 端点、12 条 conformance 断言、4 个违规变体 |
| 2026-10-22 | 1.1（N5a） | 新增 `manifest.kinds[]`（judge/process，闭集校验，缺省=judge）与 §4.4 结果行契约：`result(ok/failed/skipped)` 必填、`verdict` 仅判定类（三态 pass/fail/undetermined）、产物只准工作版本/导出文件；处理类回 verdict = 整批拒收（`row_verdict_not_allowed`）。旧 `verdicts[]` 应答逐规则 1:1 归一化，不改 api_version |
| 2026-10-21 | 1.1 | 网关补两处「算力缺口」（K07 门禁误判根因）：`/detect/bbox`（掩码→主体 bbox + 目标裁剪窗，能力沿用 `detect.u2net`）、`/detect/white_stats`（边缘白度统计，新能力 `detect.white`，复用 `engine-classic` 与旧引擎同口径）；conformance 假网关与 `GATEWAY_ENDPOINTS` 同步 12 端点；其余端点/方法/错误码不变 |
| 2026-10-22 | 1.1（N6b） | 网关补端点 `POST /op/encode`（转格式/质量档/alpha 策略/EXIF 方向摆正；能力**沿用** `op.geometric`，不新增能力、不升 api_version——升版本会让已装包的严格相等匹配失效）；`GATEWAY_ENDPOINTS` 同步 13 端点。语义：`format` 与 `out` 扩展名不一致 → 400；jpeg 无 alpha → 按 `background`（默认白）合成并回 `alpha:"flattened"`；png/webp 无损 → 回 `quality_applied:false` 不假装生效；`out` 不得等于 `path`（原图只读） |
| 2026-10-22 | 1.1 | `/detect/bbox`、`/detect/u2net` 应答**增补** `instances`（掩码全部连通域 = 多商品实例，含 `area_ratio`，宿主按 `min_instance_ratio` 默认 1% 过滤噪声）：多商品图的第二件商品自带文字不再被误判成主体外促销条（K07 f002）。纯增字段、不改既有语义：老插件不读该字段，老宿主不给该字段时插件退回单主体口径 |
| 2026-10-22 | 1.1（N6c） | 网关补**叠加**能力缺口：新端点 `POST /op/watermark` + 新能力 `op.overlay`（`{path,text,out,position?,opacity?,font_size?,margin?,color?}` → `{path,width,height,op,coverage}`），宿主复用 `core-media::watermark_text`（ab_glyph + 内嵌 Noto Sans SC/OFL，纯 Rust，不依赖 ONNX → 两种宿主形态都放行）。**纯增端点**：既有 12 端点语义一字未改，`api_version` 仍为 `1.1`——它按 §8 严格相等匹配，升版本会把所有已装 1.1 包一次性挡在门外，与「增能力是为了让新包能用」相悖；conformance 假网关与 `GATEWAY_ENDPOINTS` 同步 14 端点（N6b 的 `/op/encode` + 本端点） |
| 2026-10-26 | 1.1（i18n-F） | **可选多语言字段**（§4.0.1）：manifest `name`/`short_name`/`summary`、rules `rules[].name`、`result_columns[].label` 接受字符串（全语言通用，向后兼容）或 `{"locale": 文案}` 对象；宿主取值链 locale → zh-Hans → 任意 → 空串；请求语言 = `Accept-Language`；API 下发 `nameI18n`/`ruleNameI18n`/`labelI18n` 原样对象。**不升 `api_version`**：老包零改动照跑，conformance 12 → 14 条断言（13 对象形态可解析、14 回退链契约），`run_all.sh` 增对象形态端到端夹具 good-i18n |
| 2026-10-23 | 1.1（N19） | 处理类插件**日志/应答韧性**（真机 `white_bg_fill` 6/10 超时根因）：① 包内 `log()` 永不抛异常，stderr 失效降级写 `<STAGE_LOG>.stderr.log`；② 应答改 `os.write` 直写 fd，文本层兜底且失败留证；③ 工作线程 `except BaseException`，杜绝「没应答就结束」；④ `dup2` 后保住被换下的 stdout 引用（Windows 上对象被 GC 会连带关掉 stderr 句柄）；⑤ 单次网关调用 60s 超时 + **仅传输层**一次性重试（业务错误/`report.emit` 不重试）；⑥ 分段耗时上报（`evidence.stages` + `DAMPACK_STAGE_LOG`）与排障开关 `DAMPACK_THREAD_DUMP[_SECS]`（faulthandler 定时转储，挂起也打栈）。协议语义/端点/错误码一字未改 |

---

## 12. vNext 演进（声明未实现）

> 本节只声明方向，**不实现**；升 `api_version` 时逐项设计，勿提前开卡（plan 明确不做项）。

**轴纯度原则**：`outputs` 只编码「输出是结论还是产物」这一根轴；范式演进一律加**兄弟声明字段**——
`input_scope` = per_asset\|per_set、`artifact_identity` = working_version\|new_asset_lineage、
`row_cardinality` = 1:1\|1→M\|N→1（各独立闭集、各带缺省、走装机校验）。
**禁止往 outputs 塞场景值（select/synthesize 之类）——那是合规中心化的换马甲重犯。**

三个扩展点：

1. **集合级调用**：整集进 → 行流出，逐图是集合退化为单图的特例
   （realestate F3/F4 包内按目录自聚合 = 需求实锤）；
2. **血缘产物**：合成图 = 新资产 + `parentAssetId` 指向输入集，产物挂靠从「单工作版本」扩为
   「工作版本 \| 新资产（带血缘）」——「结果集是视图不长第二个图库」边界不破；
3. **结果行 1→M**：行外壳 `assetId` 单值变可选数组，或产物层独立于逐资产行。

未来 manifest 示例（拼图插件）：

```jsonc
{
  "outputs": ["artifact"],
  "input_scope": "per_set",
  "artifact_identity": "new_asset_lineage",
  "row_cardinality": "n_to_1"
}
```

—— `outputs` 不加值，新范式 = 新兄弟字段。
