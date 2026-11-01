# 行业合规插件化实现 Plan（多语言可插拔）

**Goal:** 把「行业合规判定 + 修复处理流程」从引擎里的 YAML 声明改为**可动态安装的插件（代码）**，宿主只提供能力（算子/检测器/VL/IO），从而支持任意语言实现插件、按行业灵活适配流程。

**Architecture:** 宿主（Rust 引擎）暴露本地能力网关（HTTP + 一次性令牌），插件是独立可执行子进程，通过 stdio JSON-RPC 与宿主交互；`.dampack` 包 = manifest + 插件二进制 + 资源；安装=解压+签名校验+能力校验+注册，运行=按需拉起子进程。

**Tech Stack:** Rust（宿主 / 官方包）、任意语言（第三方包，Python 为参考实现）、JSON-RPC 2.0、SQLite（包注册表）、ed25519（签名）。

---

## 1. 现状与问题（为什么 YAML 方案不够）

证据（`/data/projects/soujpg-dam-desktop`，main `20a16ff`）：

| 现状 | 位置 | 后果 |
|---|---|---|
| 流程是 `pipeline.yaml` 静态 `steps` 声明序 | `packs/ecommerce-taobao/pipeline.yaml` | 只能表达"同一套流程换顺序"，表达不了行业间完全不同的处理 |
| 每条规则自带 `fixer`，复检只重跑**本规则** | `crates/svc-pipeline/src/compliance_queue.rs:2175 judge_rule`、`handle_fix:2605` | 修 A4（裁切）破坏 A3/A2 后没有下一步动作 |
| `recheck_max: 1` | `pipeline.yaml` | 无法表达"裁→放大→再裁→再放大" |
| 规则语义与修复策略绑死 | `rules.yaml` 的 `fixer` 字段 | 换行业得改引擎或发明配置 DSL |

实测后果（`t_7b06d277` 复跑）：A4 阈值 0.55 下 pass 组 0 合格 —— 6 张靠 `smart_crop` 才过，但裁剪产物违反 A3 白边 / A2 尺寸，G6 复检翻回不合格。**不是阈值问题，是流程表达力问题。**

结论：流程必须是**代码**，配置只留元数据。

---

## 2. 设计总览

```
┌──────────────────────── 宿主（Rust，damdesk/engine） ────────────────────────┐
│  pack-host（新 crate）                                                       │
│   ├─ install/registry：.dampack 解压、签名校验、能力校验、SQLite 注册          │
│   ├─ process：子进程生命周期（懒启动、health、崩溃重启×2、冷却）               │
│   └─ gateway：本地 HTTP 能力网关 127.0.0.1:<port> + 一次性 Bearer 令牌         │
│        └─ /op/*  /detect/*  /vl/*  /io/*  ← 复用现有算子与 VL 客户端          │
└───────────────┬──────────────────────────────────────────────────────────────┘
                │ stdio JSON-RPC（语言无关）
┌───────────────▼───────────────┐   ┌──────────────────────────────────────┐
│ 插件进程 A（Rust 官方包）      │   │ 插件进程 B（Python 第三方包）          │
│  judge() / repair() 任意逻辑   │   │  同一协议，同一 conformance 套件       │
└───────────────────────────────┘   └──────────────────────────────────────┘
```

三条硬边界：
1. 插件**不直接**联网、不解大图、不遍历磁盘 —— 全部走网关传的 asset 路径与算子端点。
2. 判定/修复逻辑、步数、顺序、分支全在插件代码里，宿主不解释流程。
3. 每次结果落 `packId@version + pluginHash + deterministic`，保证可解释、可追责。

---

## 3. 关键契约

### 3.1 插件协议（stdio JSON-RPC 2.0，每行一个 JSON）

| 方法 | 参数 | 返回 |
|---|---|---|
| `pack.info` | — | `{id, version, api_version, deterministic, capabilities}` |
| `health` | — | `{ok, uptime_ms}` |
| `judge` | `{jobId, assetId, path, targets{}, options{}}` | `{verdicts:[Verdict], deterministic}` |
| `repair` | `{jobId, assetId, path, verdicts[], targets{}}` | `{outputs:[{path, actions[]}], conclusion, notes{}}` |
| `report` | `{assetId, verdicts[], outputs[]}` | `{manifestFields{}, columns[]}` |
| `cancel` | `{jobId}` | `{}` |

`Verdict`：
```json
{"ruleId":"A4_subject_ratio","status":"pass|fail|undetermined|skipped",
 "score":0.62,"label":"商品主体占比","evidence":{"fgRatio":0.62,"bbox":[..]},
 "fixed":false,"note":"..."}
```

错误码：`-32001` 能力缺失、`-32002` 资产不可读、`-32003` 参数非法、`-32004` 取消、`-32005` 超时、`-32000` 其他。

约定：
- 插件 stdout **只**输出协议行；日志一律 stderr（Python 需 dup 保护，见 §5）
- 宿主对 `judge`/`repair` 设超时（默认 120s，可按包 manifest 覆盖），超时 → 发 `cancel` → 杀进程 → 记 `undetermined(plugin_timeout)`

### 3.2 宿主能力网关（插件通过 HTTP 调）

所有请求带 `Authorization: Bearer $DAM_TOKEN`；宿主按 `capabilities` 白名单放行。

| 端点 | 入参 | 出参 | 复用实现 |
|---|---|---|---|
| `POST /io/read` | `{path}` | `{width,height,exif{}}` | `core-media` |
| `POST /detect/u2net` | `{path}` | `{maskPath, fgRatio, bbox, bgDev}` | `engine-segment` + `compliance_queue` 现算路径 |
| `POST /detect/ocr` | `{path, lang}` | `{boxes:[{text,box,conf}]}` | `engine-ocr` |
| `POST /op/crop` | `{in, box, out}` | `{path}` | 新增（smart_crop 已有等价实现） |
| `POST /op/letterbox` | `{in, out, targetShare, bg}` | `{path}` | 新增（补白到占比目标） |
| `POST /op/upscale` | `{in, out, minShortSide, maxScale}` | `{path, scale, real:false}` | 现有 upscale，需回 `real` 标记 |
| `POST /op/replace_bg` | `{in, out, bg}` | `{path}` | 现有 `replace_bg` fixer |
| `POST /op/inpaint` | `{in, out, box, prompt?}` | `{path}` | 现有 inpaint |
| `POST /vl/judge` | `{path, prompt, schema, maxTokens?}` | `{json, contentEmpty, reasoningLen}` | `svc-pipeline::compliance_vl` |
| `POST /report/emit` | `{jobId, fields{}}` | `{}` | 导出 manifest 落库 |

### 3.3 manifest.json（唯一的配置面）

```json
{
  "id": "ecommerce-amazon",
  "name": "电商主图合规（Amazon）",
  "version": "1.0.0",
  "api_version": "1.0",
  "entrypoint": {"cmd": "bin/dam-pack-ecommerce", "args": []},
  "capabilities": ["io.read","detect.u2net","op.crop","op.letterbox","op.upscale","op.replace_bg","vl.judge"],
  "deterministic": false,
  "timeout_secs": 120,
  "permissions": {"read": ["assets"], "net": ["host-gateway"]},
  "signature": "ed25519:<base64>",
  "resources": ["resources/models/u2net.onnx"]
}
```

### 3.4 包格式与安装目录

```
ecommerce-amazon.dampack (zip)
├── manifest.json
├── bin/dam-pack-ecommerce(.exe)
├── resources/...
└── README.md
```
安装目录：`<appdata>/dam/packs/ecommerce-amazon/1.0.0/`，`packs` 表记 `(id, version, hash, enabled, installed_at, signature_status)`，`current` 指针单独存，回滚=切指针。

### 3.5 结果与可追溯

- `verdict` 与 `run` 记录必带 `pack_id / pack_version / plugin_hash / deterministic`
- `deterministic: false` 的包：结论标记「建议」，不进「自动合格」的最终结论（需人工确认或被引擎二次规则裁定）
- 历史结果**不重算**：换包/升级后旧结果保留原版本标记，对比视图并列展示

---

## 4. 行业适配示例（流程差异由代码表达，不用配置）

| 行业包 | judge | repair | mode 语义 |
|---|---|---|---|
| 电商白底主图 | 7 条几何+语义规则 | 语义先（去叠加/换白底）→ 几何一次求解（crop→letterbox→upscale）→ 自复检 | 自动修复，产物标"修复后合格" |
| 房地产/家居 | 尺寸/占比/真实性 | 仅几何（裁+缩），**禁止**生成式 | geometric_only + 强制转人工复核 |
| 食品/医疗/美妆 | 法规文本规则 + VL | 不修（只给整改建议） | advisory |
| 广告/设计素材 | 品牌规范 | 允许生成式编辑（局部 inpaint，不改主体） | generative（allow_redraw_subject=false 默认） |

关键：这些差异在**各插件源码里**，宿主零改动。

---

## 5. 多语言支持策略

| 角色 | 语言 | 交付形态 | 说明 |
|---|---|---|---|
| 官方行业包 | Rust | 单二进制 | 直接依赖 `core-media`/`engine-segment` crate，走进程内调用可省 IPC；也可仍走网关，二选一由 manifest 声明 |
| 第三方/合作方包 | Python | PyInstaller 单文件（或 uv 自包含） | 参考实现随 SDK 提供，含 `gateway.py` 客户端 |
| 其他语言 | 任意 | 可执行 + stdio | 只依赖协议，不依赖 SDK |
| 一致性测试 | Python | `tools/pack-conformance/` | 假网关 + 12 条协议断言，任何语言的包都要过 |

Python SDK 必含（`sdk/python/dam_pack/`）：
- `runtime.py`：RPC 循环 + stdout dup 保护 + 错误码映射
- `gateway.py`：能力网关客户端（urllib，零依赖）
- `schema.py`：Verdict/manifest 数据类
- `testing.py`：假网关（mock 端点）供插件单测

---

## 6. 里程碑与验收

### M0 契约原型（1 天）
- 交付：`docs/pack-protocol.md`（协议定稿）+ `sdk/python/dam_pack/runtime.py` 骨架
- 验收：用假网关跑通 `pack.info` / `health` / `judge` 三方法；`tools/pack-conformance` 12 条断言全绿（对 mock 插件）

### M1 宿主网关 + Rust 电商插件（行为等价，3~4 天）
- 新增 `crates/pack-host/`：`manifest.rs`、`gateway.rs`（axum 本地服务）、`process.rs`
- 新增 `plugins/ecommerce-amazon/`（Rust bin，`IndustryPack` trait 实现）
- 宿主侧 `svc-pipeline` 保留 judge/fixer 为**算子**，不再由 pack YAML 驱动
- 验收（硬指标）：
  1. 20 张语料逐图 `ruleId + status + score` 与现引擎一致（容差 0，VL 规则按 `deterministic:false` 单独列）
  2. 插件被 `kill -9` 后宿主任务落 `undetermined(plugin_crash)`，进程不退出、后续任务正常
  3. 单图端到端耗时 ≤ 现引擎 ×1.2
  4. A4 修复链（crop→letterbox→upscale）产物同时满足 占比≥0.55 且 短边≥1600 且 纯白底（G6 复检通过）

### M2 动态安装 / 签名 / 回滚（2~3 天）
- `pack-host/install.rs`、`registry.rs`：解压、sha256、ed25519 验签、`api_version` 兼容、`capabilities` 校验
- 验收：装/升级/回滚/卸载 4 条路径各自可验证；坏签名拒绝；缺能力拒绝并在 UI 给出缺哪项；两版本并存切 current 生效

### M3 Python SDK + conformance（2 天）
- Python 电商插件（同流程、行为对齐 Rust 版）
- 验收：Python 包过同一 conformance 12 条；20 张矩阵结果与 Rust 版一致（VL 规则按确定性口径单独对比）

### M4 第二行业插件验证"宿主零改动"（2 天）
- `plugins/realestate-cn/`（geometric_only + 转人工）
- 验收：`git diff --stat` 证明宿主 crate 改动文件数 = 0；只新增插件目录 + 安装即可跑

### M5（可选，先不做）市场/第三方审核/权限细分
- 触发条件：确有外部开发者接入。YAGNI，先不设计。

---

## 7. 任务拆分（可建卡粒度，串行链）

| # | 卡 | 产出 | 依赖 |
|---|---|---|---|
| 1 | PACK-0 协议定稿 + conformance 骨架 | `docs/pack-protocol.md`、`tools/pack-conformance/` | — |
| 2 | PACK-1 宿主 `pack-host` crate（manifest+process+gateway） | `crates/pack-host/*` + 单测 | 1 |
| 3 | PACK-2 电商插件（Rust，行为等价） | `plugins/ecommerce-amazon/` | 2 |
| 4 | PACK-3 20 张矩阵等价验证 + 性能基线 | `reports/PACK-EQUIV.md` + 证据 | 3 |
| 5 | PACK-4 安装/签名/回滚 | `pack-host/install.rs`、`registry.rs` | 2 |
| 6 | PACK-5 Python SDK + Python 插件 | `sdk/python/`、`plugins/ecommerce-amazon-py/` | 1,3 |
| 7 | PACK-6 房产插件（验证宿主零改动） | `plugins/realestate-cn/` | 4,5 |

---

## 8. 与现有代码的关系

| 现有 | 处置 |
|---|---|
| `packs/*/pipeline.yaml` 的 `steps`/`depends_on` | 迁移期保留（旧引擎可跑）；插件上线后标记 deprecated，流程以插件代码为准 |
| `packs/*/rules.yaml` | 保留为**规则文档/展示元数据**（UI 规则列表、报告列、fail_label），不再驱动执行 |
| `compliance_queue.rs::judge_rule` / `handle_fix` | 降级为宿主算子（供网关调用），不再承担流程编排 |
| `compliance_vl.rs` | 保留为 `/vl/judge` 实现；`default_max_tokens=4096`、失败落 `undetermined` 的口径保持不变 |
| `recheck_max` | 由插件自己决定复检次数，配置项废弃 |

迁移策略：双跑对比期（旧引擎 + 插件各跑同一批 20 张），逐图比对通过后才切默认；不做一次性硬切。

---

## 9. 风险与开放问题

| 风险 | 处置 |
|---|---|
| 插件不确定性（VL）导致结果漂移 | manifest 标 `deterministic:false`；结论只作建议；矩阵对比按"多跑 3 次统计一致性"评估 |
| 插件崩溃拖垮任务 | 子进程隔离 + 重启×2 + 冷却；崩溃记 `undetermined(plugin_crash)`，不冒充失败或合格 |
| 网关被本机其他进程滥用 | 只绑 127.0.0.1；每次启动生成一次性令牌；令牌随进程退出失效 |
| Windows 杀软误报（Python 包） | 官方包用 Rust；Python 包要求签名 + 安装目录白名单说明 |
| 插件体积（自带模型） | 模型优先由宿主资源提供，插件只声明依赖；确需自带则 manifest `resources` 列出并计入安装体积提示 |
| API 版本演进 | `api_version` semver；宿主只加载兼容区间内的插件；不兼容时 UI 明确提示"需升级插件" |
| 结果不可比（跨版本） | 结果落 `pack_id@version+hash`；报告与对比视图按版本分组 |
| 并发与进程池 | 首版一 job 一进程；若启动开销成为瓶颈再上池化（YAGNI，先不设计） |

---

## 10. 明确不做（YAGNI）

- 不做 WASM 沙箱（首版）
- 不做插件市场、在线分发、自动更新
- 不做插件热重载
- 不做通用"修复流程编排 DSL"（这正是要摆脱的东西）
- 不做跨机器插件同步

---

## 11. 验收总纲（贯穿所有里程碑）

1. **能力边界**：插件无法直接联网/读任意路径（用 `strace`/权限测试证明）
2. **等价性**：切换插件前后，20 张语料逐图结论一致（VL 规则按确定性口径单列）
3. **可追溯**：任意一条历史 verdict 都能查到判它的 `pack@version+hash`
4. **宿主零改动**：新增一个行业 = 新增一个插件目录，宿主代码 0 行改动
5. **失败可见**：崩溃/超时/缺能力/坏签名，全部落明确状态与文案，不出现"静默合格"
