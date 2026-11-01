# 行业包 → 行业插件 迁移落地 Plan（2026-10-20 定稿）

架构裁决：`2026-10-20_industry-compliance-plugin-arch.md`（流程 = 代码插件，宿主只提供能力）。
本文件是**唯一权威的工程落地计划**（形态 / 阶段 / 验收门 / 历史卡处置 / 卡号），此前所有增补条目已合并入正文。

---

## 0. 一页结论

```
damdesk (Rust, 底层不动)
 ├─ 共享 python runtime（python-build-standalone，仅解释器 + 标准库，不含第三方库）
 ├─ 每个行业包独立 venv：<runtime>/python -m venv <pack>/venv
 │      └─ <pack>/venv/bin/pip install -r requirements.txt
 ├─ 插件子进程：<pack>/venv/bin/python -m dampack_launcher <pack_dir>
 │      └─ stdio JSON-RPC（pack.info / health / judge / repair / report / cancel）
 └─ 能力网关（127.0.0.1 + 一次性 Bearer + capabilities 白名单）
        /io/read · /detect/{u2net,ocr} · /op/{crop,letterbox,upscale,replace_bg,inpaint} · /vl/judge · /report/emit
```

三句话：
1. **插件是 Python 源码，不编译**——官方包与合作伙伴包同一形态，改插件 = 改 .py。
2. **依赖环境是每包一个 venv**——不是手工拼 sys.path，不是 vendor 覆盖共享目录。
3. **进程隔离 + 能力网关**——不做 ComfyUI 式进程内 import；模型与 VL key 只在宿主。

不变：判定面（检测器 / 三态 verdict / 人工复核 / 组级传导 / 反馈 / 报告）、能力实现（u2net / OCR / lama / realesr / core-media）、UI 骨架。
变：谁编排流程（YAML → 插件代码）、包形态（四件 YAML → .dampack）、包管理 UI、DB 一表两列。

---

## 1. 现状锚点（实测）

| 面 | 位置 | 现状 |
|---|---|---|
| 执行层 | `crates/svc-pipeline/src/compliance_queue.rs` | `judge_rule:2181`、修复复检环 `:2577`（只复检本规则，受 `recheck_max` 限制） |
| 包装载 | `crates/core-config/src/packs.rs` | `Pack::from_dir` 读四件 YAML |
| 导入落盘 | `crates/app-web/src/compliance_api.rs:694` | 校验四件 → `{data_dir}/packs/<id>/` |
| 包端点 | `compliance_api.rs:1856-1859` | `GET /packs`、`GET /packs/{id}`、`POST /packs/activate`、`POST /packs/import` |
| 状态 | `compliance_api.rs:200` | 返回 `activePack` + `packs{builtin,imported}` |
| UI | `web/src/components/PackManager.tsx`（296 行，挂 `App.tsx:152`） | 列表 / 只读预览 / 设为激活 / 导入目录 |
| DB | migrations 至 `0024` | 无 `packs` 表；verdict 无插件来源列 |
| VL 通道 | `core-config/src/lib.rs:290-310` | provider preset：deepseek(deepseek-flash) / dashscope / zhipu(glm-*) / ollama / custom；`compliance_vl` 为 OpenAI 兼容（base_url + api_key + model） |

---

## 2. 插件包规范

```
<id>.dampack (zip)
├── manifest.json
├── plugin/__init__.py        # 导出 PACK_INFO / JUDGE / REPAIR
├── plugin/...                # 其余实现文件
├── requirements.txt          # 可空；装进本包 venv
└── resources/                # 规则文档、语料示例、报告模板
```

manifest（唯一配置面）：
`{id, name, version, api_version, entrypoint{cmd,args}, capabilities[], deterministic, timeout_secs, permissions{read,net}, signature, resources[]}`

加载语义（参考 ComfyUI custom_nodes，但运行在子进程内）：

1. **安装**：解压 → sha256 → ed25519 验签 → `api_version` 兼容 → `capabilities` 齐备 → **建 venv 并 pip install** → 写 `packs` 表。
2. **启动**：宿主 spawn `<pack>/venv/bin/python -m dampack_launcher <pack_dir>`；launcher 保护 stdout（协议与日志分离）、Windows `os.add_dll_directory`、import `plugin`、读入口；异常非零退出 + stderr 原因。
3. **失败隔离**：import 失败 → `plugin_load_failed(<原因>)`；超时 → cancel → kill → `undetermined(plugin_timeout)`；崩溃 → 重启 ×2 → 冷却 → `undetermined(plugin_crash)`。宿主永不退出，绝不把 undetermined 当 pass。
4. **更新**：kill 旧进程 + 启新进程即可，**不需要重启 damdesk**（优于 ComfyUI 需重启服务）。

依赖隔离（2026-10-20 再修正，覆盖早前 sys.path + vendor 方案）：

- 共享 runtime **只当解释器**，不装第三方库；**禁止 `--system-site-packages`**。
- 每包独立 venv；不手工拼 sys.path、不设 shared-site、不做 vendor 覆盖。
- 理由：进程隔离只解决全局状态；手工 sys.path 会拼出「包自带 numpy 1.26 + 共享半套要求 2.x」这类未测试混合环境，原生库还有 ABI / DLL / OpenMP 借用问题。
- 代价：磁盘按包重复（宿主 pip `--cache-dir` 缓解）+ 安装期几十秒；收益：每个包是完整可测、可冻结、可整删的环境。

能力访问（唯一通道：插件不读模型文件、不见 key、不读设置库）：

| 插件要做 | 端点 | 宿主侧实现 | 返回 |
|---|---|---|---|
| 读原图 | `/io/read` | 校验 assetId / 路径 | 字节或临时路径 |
| 主体分割 / 占比 | `/detect/u2net` | engine-segment | 掩码 / fg_ratio |
| 文字 / 水印定位 | `/detect/ocr` | engine-ocr | `[{box,text,conf}]` |
| 裁剪 / 补白 / 缩放 | `/op/{crop,letterbox,upscale}` | core-media | 产物路径 + 宽高 |
| 换底 | `/op/replace_bg` | 宿主修复器 | 产物路径 + 宽高 |
| 擦水印 / 补全 | `/op/inpaint` | 宿主 lama（K13 补下载） | 产物路径 + 宽高 |
| VL 判定 / 打标 | `/vl/judge` | 用户设置的 provider（deepseek-flash / dashscope / zhipu / ollama），宿主持 key | `{answer, evidence, model, latencyMs}` |
| 交证据 / 报告 | `/report/emit` | 宿主落库 | ok |

并发与配额：网关按能力排队（inpaint / upscale 限并发），防多插件抢算力；evidence 记模型 id + 版本 + provider + 耗时，保证可复现、可追责。

---

## 3. 阶段划分（串行，每阶段有硬门）

### 阶段 A（M0）协议定稿 + conformance —— 不碰产品代码

- `docs/industry-pack-protocol.md`：stdio JSON-RPC 2.0；方法 `pack.info/health/judge/repair/report/cancel`；`Verdict{ruleId,status,score,label,evidence,fixed,note}`；错误码 -32001~-32005；stdout 只走协议、日志走 stderr。
- `tools/pack-conformance/`：假网关 + 12 条断言 + 违规变体（stdout 污染 / 缺方法 / 错误码乱用 / 不响应 health）。
- 门：违规变体逐条报错；参考实现 12/12。

### 阶段 B（M1）宿主执行层 —— UI 不动

- 新 crate `crates/pack-host`：
  - `manifest.rs`：解析 + 校验（未知键报错、api_version 兼容、capabilities 齐备）
  - `process.rs`：懒启动 → health 握手 → 超时 cancel → kill(-TERM→-KILL) → 崩溃重启 ×2 → 冷却
  - `gateway.rs`：端点 + 一次性 Bearer + capabilities 白名单（未声明 403）
  - `runtime.rs`：共享 python runtime 定位与版本校验；`tools/dampack_launcher.py`（stdout 保护、add_dll_directory、入口 import、失败原因）
- `crates/svc-pipeline`：judge / repair 改插件调用；`judge_rule` / `handle_fix` 降级为网关算子（逻辑保留，含 VL 4096 预算与 undetermined 口径）；`pipeline.yaml` 的 `steps` / `recheck_max` 废弃（保持可解析，不 panic）。
- 官方电商包（Python）：`packs/plugins/ecommerce/`，六规则（R0 领域 gate / R1 尺寸 / R2 白底 / R3 无水印 / R4 占比 / R5 打标）+ **geometry_solve**（crop → letterbox → upscale 一次闭式求解）+ 每轮全量复判，≤3 轮后转人工。
- `crates/app-web`：`pack_list` 返回内置插件；`status` 增 `pluginState`。
- DB：`0025_packs_registry.sql`（packs 表）、`0026_verdict_pack_source.sql`（verdict 增 packVersion / pluginHash）。
- 设置项 `compliance_engine = legacy | plugin`，默认 legacy（双跑开关）。

门（四条全过才往下）：

1. **10 张**语料逐图 `ruleId+status+score` 与旧引擎基线一致（VL 规则按 `deterministic:false` 单列）；
2. `kill -9` 插件 → `undetermined(plugin_crash)`，宿主不退出；
3. 单图耗时 ≤ 旧引擎 ×1.2（同机同语料）；
4. A4 修复链产物同时满足 占比≥0.55 + 短边≥1600 + 纯白底，G6 复检通过。

### 阶段 C（M2）安装 / venv / 验签 / 回滚 + UI 换形态

- 后端：`.dampack` 安装（解压 → sha256 → 验签 → api_version → capabilities → **建 venv + pip install** → 写表）；`current` 指针独立存，回滚 = 切指针，旧版本目录保留。
- 端点：`POST /packs/install`、`POST /packs/{id}/activate`、`POST /packs/{id}/rollback`、`DELETE /packs/{id}`（内置不可卸）、`GET /packs`（版本 / 运行态 / 签名 / 是否内置）、`GET /packs/{id}`（manifest + 规则文档，来自 `pack.info`）。
- 前端 `PackManager.tsx`（296 → 约 350 行）：
  - 列：id / 名称 / 版本 / 来源（内置·安装）/ 运行态（running·冷却·崩溃）/ deterministic / 签名（未签·通过·失败）/ 规则数
  - 动作：设为激活 / 预览（manifest + 规则只读）/ 回滚 / 卸载（内置禁用）
  - 底部：安装 `.dampack`（文件选择，替换原「导入目录」）；验签失败与能力声明显式展示
  - 不变：激活唯一性、坏包照列并报错、换插件不删判定历史；删除「规则可编辑」入口
- 合规页增量：verdict 加 `packId@version` 来源标记；`undetermined` 增加 `plugin_timeout` / `plugin_crash` 文案。
- 门：装 → 激活 → 判定 → 回滚 → 卸载全通；坏包三例（sha256 不符 / api_version 不兼容 / capabilities 缺项）各自明确错误码；同版本重复安装幂等；前端 RTL 测试 + 构建 PASS。

### 阶段 D（M3/M4）多语言与零改动验证

- Python SDK `tools/pack-sdk-python/`：**官方包即用该 SDK 编写**（网关客户端、Verdict、错误码、stdout 保护、venv / 依赖声明）。
- 房产包 `packs/plugins/realestate/`：仅几何、禁生成式、无法几何修复时强制转人工。
- 门：`git diff --stat` 对宿主 crate 与 `web/` 为空（宿主零改动）；conformance 12/12。

### E2E（推迟，2026-10-20 用户裁决）

UI 集成（headless 全流程）、出包（NSIS）、真机（Lenovo Windows）三张卡已放行自动执行（K15 链末在 Windows 真机收尾）。

真机发现问题处置（2026-10-20 裁决）：先记 `damdesk-repo/references/known-issues.md`（编号 FIX-n，父卡 = K15），再**自动建修复卡**（`--parent K15`，workspace=worktree，验收 = 修复 + 单测 + L0，不要求再装真机）；同一根因合并一张；阻塞测试流程的项仍先记录+建卡，再 `block(kind=needs_input)`；禁止「只写报告不建卡」或把失败项写成通过。

---

## 4. 验收总表

| 类别 | 口径 |
|---|---|
| 单元测试 | `cargo test --workspace` 全绿；`pnpm -C web test` 全绿；conformance 12/12 |
| 等价 | 10 张语料逐图一致（VL 单列）；错误/崩溃场景落 undetermined，不冒充 pass/fail |
| 性能 | 单图 ≤ 旧引擎 ×1.2 |
| 体积 | 安装包 +1 份 runtime；每个官方包 <2MB |
| 可诊断 | 坏包 / 加载失败 / 模型缺失 / 超时 各有明确错误码与 UI 文案 |

---

## 5. 历史 kanban 卡整合（soujpg 板，实测 375 张：done 332 / archived 38 / blocked 4 / todo 1）

**沿用**（代码与资产直接进插件体系，不重跑）：C0–C9 十张（判定面、检测器、修复器代码保留，阶段 B 只换执行路径；C3/C4 卡内「插件」改称**网关算子**）；迁移 0015/0016/0017/0018 + CMP-JOB-01/02；A 系列 78b216b1 / 3b81c19b / bc8c943b / 7b06d277（语义修正进算子）；ECOM-PACK-1 语料（取前 10 张作基线）；INPAINT-K3 50 张基线；DT5 OCR/u2net 内置；合规 UI 系列（cef05116 / abe11f98 / cc9d5992 / c49a8ad5）。

**封存**（卡留历史，不再按原口径推进）：C2 管道编排（433a42aa，`steps`/`recheck_max` 废弃）、C3 smart_crop（4ba16055）、C4 修复三件 + do 五件（0a2bbc94）、C7 房产 YAML spike（e6cf599f）、ECOM-PACK-2 / 2B / 3（327ce2e2 / c3dd20af / cbef633a）。

**已改写并跑完**：`t_d190732b`（ECOM-PACK-4-WIN-VERIFY）按「旧引擎 Windows 基线 + 10 张 + VL=deepseek-flash」执行，23:19 done，结论只作等价对照；插件版真机验收归 K15。

**注意**：3b81c19b 的「跨规则复检」是**单向传递**（修复产物喂后续规则），不回检已判规则；A4 破坏 A1/A3 的问题域由 geometry_solve + 每轮全量复判 + 硬门 4 覆盖。

---

## 6. 卡号与依赖（16 卡 / 18 边；soujpg 板；pin deepseek-flash + reasoning high）

```
K00 t_7aba77f3 基线（10 张）               [done 23:20]
K01 t_cb75c8ed 协议 + conformance
K02 t_5118ce43 pack-host（manifest/process/gateway/runtime + launcher）
K03 t_5548567f 能力网关
K04 t_ff4780ed 电商包（Python 源码 + geometry_solve）
K05 t_06c17b35 切执行路径 + DB 0025/0026 + 双跑
K06 t_3e9c3028 测试体系
K07 t_dc40d71f 等价门禁（四条硬指标）
K08 t_fea33411 安装 / venv / 验签 / 回滚
K09 t_503a7ce7 PackManager 换形态
K10 t_77ba4297 UI 集成 E2E                [E2E 推迟]
K11 t_f537e936 Python SDK（官方主路径）
K12 t_2653e132 房产包 + 宿主零改动
K13 t_6e8888ca lama 模型下载 / 内置
K14 t_f1c28aa8 出包（含体积预算）         [E2E 推迟]
K15 t_a0fb0fd8 真机 E2E                   [E2E 推迟]
```

链设计：`2026-10-20_industry-plugin-kanban-chain.md`；建卡脚本：`create_industry_plugin_chain.py`（幂等 `--idempotency-key`，建完对账 `task_links`）。

---

## 7. 风险与回滚

- 双跑开关 `compliance_engine=legacy|plugin`：任一阶段出问题一键切回，不影响在用判定。
- 插件崩溃 / 超时 → undetermined + 明确原因，绝不冒充 pass/fail。
- 已有手改 YAML 的包：M2 提供一次性转换（YAML → 插件 resources）或明确告知规则不可再编辑。
- Windows 杀软对 Python 进程的误报：错误态可诊断（安装即报「启动失败」，不静默）；runtime 只需一次签名。

## 8. 明确不做

WASM 沙箱、插件市场 / 在线分发、热重载、通用修复编排 DSL、跨机插件同步、进程内 Python 加载（ComfyUI 式）。

## 9. 保留代价（不掩盖）

1. 子进程仍能读盘 / 联网——「不直接联网 / 不遍历磁盘」是契约，机制化要等沙箱（job object / WASM）。
2. 每包首次启动 300~800ms（懒启动 + 常驻摊薄）。
3. 磁盘按包重复（每包 venv）。

## 10. 决策记录（时间线）

1. 流程必须是可动态安装的代码插件；YAML / DSL 两次被否（太复杂 / 表达力不足）。
2. 对比进程内 trait、进程内脚本（rhai）+ 能力阶梯、子进程插件——终裁**子进程**：合作伙伴自带处理能力是定位内需求，宿主零改动是硬目标。
3. 插件语言从 Rust 二进制改为 **Python 源码不编译**（降低合作方门槛、同一份包跨平台），加载语义参考 ComfyUI custom_nodes。
4. 依赖隔离从 sys.path + vendor 改为 **每包独立 venv**（手工 sys.path ≠ 环境隔离）。
5. 测试语料上限 **10 张**；VL 判定统一 **deepseek-flash**（禁 glm 系）。
6. E2E（UI 集成 / 出包 / 真机）推迟，先做到代码 + 单元测试全绿。
