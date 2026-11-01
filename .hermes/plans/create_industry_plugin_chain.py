#!/usr/bin/env python3
"""行业插件迁移链 —— kanban 建卡脚本（soujpg 板）。

用法:
  python3 .hermes/plans/create_industry_plugin_chain.py --dry-run     # 只预览
  python3 .hermes/plans/create_industry_plugin_chain.py               # 建全链
  python3 .hermes/plans/create_industry_plugin_chain.py --only K07    # 只建某张（父卡必须已建）

规则（kanban-orchestration / chain-soujpg）:
- CARDS 与 PARENTS 分开维护；父关系缺失立即抛错，绝不静默建无父卡；
- 拓扑序创建，--parent 一次给全（带 parent 的卡初始 todo，不会被 dispatcher 秒拉）；
- 长正文一律 --body-file；--json 解析整个 stdout（多行 pretty-print）；
- 建完对账：链式卡不得出现在 ready；task_links 与 PARENTS 逐边比对，差异非零退出。
"""
import argparse
import json
import os
import sqlite3
import subprocess
import sys
import tempfile

BOARD = "soujpg"
PROFILE = "soujpg"
MODEL = "deepseek-flash"
PROVIDER = "custom"
REASONING = "high"
REPO = "/data/projects/soujpg-dam-desktop"
WS_WT = f"worktree:{REPO}"
WS_DIR = f"dir:{REPO}"
BOARD_DB = os.path.expanduser("~/.hermes/kanban/boards/soujpg/kanban.db")

ENV = """## 环境事实（写死，勿再自行侦察）
- 宿主仓库：/data/projects/soujpg-dam-desktop（分支 main）；容器 dam-dev 内 /workspace = 同一目录。
- 本卡工作区：{wt}（worktree 卡容器内是 /workspace/.worktrees/<task_id>；dir 卡即 /workspace）。
- 宿主没有 node/pnpm，一切构建进容器：
  docker exec -e CARGO_TARGET_DIR=/cargo-target dam-dev bash -lc 'cd <ws> && cargo ...'
  docker exec dam-dev bash -lc 'cd <ws> && pnpm install --prefer-offline && pnpm -C web test'
- 报告落宿主 /data/projects/soujpg-dam-desktop/reports/<task_id>-*.md（容器内同一路径 /workspace/reports/），不要写进 worktree。
- 内网 push 走 `git -c http.proxy= push`；禁止 `git add -A`，只提交本卡白名单内文件。
- 口径（2026-10-20 用户裁决）：VL 判定通道统一 **deepseek-flash**（OpenAI 兼容 https://api.deepseek.com/v1，provider preset=deepseek），禁止 glm-*；**测试语料上限 10 张图**（分母=10，不得放大）。
- L0 门禁每卡必跑：docker exec -e CARGO_TARGET_DIR=/cargo-target dam-dev bash -lc 'cd <ws> && bash scripts/test_gate.sh'，卡里贴 `L0 门禁：PASS <耗时>`。
"""


# 2026-10-20 口径覆盖：插件用 Python 源码（不编译），宿主共享一份 runtime，仍走子进程。
OVERRIDES = {
    "K02": "## 【2026-10-20 口径覆盖】插件用 Python 源码不编译；宿主带一份 python-build-standalone 共享 runtime；\n"
           "每包独立 venv（<runtime>/python -m venv <pack>/venv + venv/pip install -r requirements.txt；共享 runtime 只当解释器，禁 --system-site-packages），\n启动 `<pack>/venv/bin/python -m dampack_launcher <pack_dir>`；launcher 负责 stdout 保护与 Windows add_dll_directory、\n"
           "import 失败非零退出 → 宿主记 plugin_load_failed。\n\n",
    "K04": "## 【2026-10-20 口径覆盖】插件改为 Python 源码包 `packs/plugins/ecommerce/`（manifest.json + plugin/ + requirements.txt + resources/），\n"
           "不再是 Rust 二进制；判定/几何求解/复判逻辑与验收不变。\n\n",
    "K11": "## 【2026-10-20 口径覆盖】Python SDK 升格为官方主路径：官方电商/房产包即用该 SDK 编写，需含网关客户端、Verdict、错误码、\n"
           "stdout 保护与 venv/依赖声明（每个包独立 venv，不做 vendor 覆盖）。\n\n",
    "K12": "## 【2026-10-20 口径覆盖】房产插件同为 Python 源码包 `packs/plugins/realestate/`；宿主零改动验收不变。\n\n",
    "K14": "## 【2026-10-20 口径覆盖】新增体积预算验收：安装包 = 1 份 python runtime + 每官方包 <2MB。\n\n",
}

HONEST = """## 诚实门
未实测的项必须标「未验证」，不得写成通过；失败要给原始输出（命令 + 关键日志行），不许只写结论。
最后一次失败时自己 kanban_block(kind='needs_input', reason=<结构化报告：结论/已尝试/根因/需决策/可复现/产物>)，并 comment 细节 + attach 产物。
"""

CARDS = {}

CARDS["K00"] = dict(
    title="【IP-K00】基线：环境准备 + 旧引擎 10 张判定基线快照",
    workspace=WS_DIR,
    max_runtime="30m",
    skills=["damdesk-repo"],
    body="""# IP-K00 基线：环境准备 + 旧引擎 10 张判定基线

## 目标
为整条插件链提供「旧引擎判定基线」：10 张语料逐图 {asset, ruleId, status, score, packId} 快照 JSON。
本卡不实现插件、不改业务代码。

""" + ENV.format(wt="主仓") + """
## 步骤
1. `git -C /data/projects/soujpg-dam-desktop status --short` 认领现状：
   - 有未提交改动时（实测曾有 9 个：crates/core-config/src/lib.rs、crates/dev-e2e/src/bin/*.rs），判定归属——
     属于既有修复的收尾就提交到 main（只 add 这些文件）；无法判定则把清单写进本卡 summary，**不要 stash/reset**。
2. 容器可用性：`docker ps --format '{{.Names}}' | grep -x dam-dev`，并打印 cargo/node/pnpm 版本。
3. 写可复用脚本 `scripts/pack_baseline_capture.py`：跑旧引擎判定（复用现有 `crates/dev-e2e/src/bin/c7_spike.rs`
   或 `scripts/c7_acceptance.sh` 的 driver 路径），对 10 张语料逐图产出 JSON 数组。
   语料来源：ECOM-PACK-1（t_6bb98335）落盘的语料，**只取前 10 张**（见该卡 summary / reports/）；找不到时用
   `packs/ecommerce-taobao` 对应 fixture，并在报告里写明实际来源。
4. `python3 scripts/pack_baseline_capture.py --out reports/<task_id>-baseline.json` 产出 >=10 条。
5. 跑 L0 门禁。

## 验收
1. 命令 `python3 scripts/pack_baseline_capture.py --out reports/<task_id>-baseline.json` → 退出码 0；
2. `python3 -c "import json;d=json.load(open('reports/<task_id>-baseline.json'));print(len(d))"` → >= 10；
3. L0 门禁 PASS（贴耗时）；
4. 基线 JSON attach 到本卡，summary 写明语料来源、脏改动处置。

""" + HONEST,
)

CARDS["K01"] = dict(
    title="【IP-K01】插件协议定稿 + conformance 骨架（不碰产品代码）",
    workspace=WS_WT,
    max_runtime="60m",
    skills=["damdesk-repo"],
    body="""# IP-K01 协议定稿 + conformance 骨架

## 目标
冻结「语言无关插件协议」，并给出可执行的 conformance 套件；本卡不碰产品代码（只写 docs/ 与 tools/）。

""" + ENV.format(wt=WS_WT) + """
## 交付
1. `docs/industry-pack-protocol.md`：
   - 传输：stdio JSON-RPC 2.0，每行一个 JSON；stdout 只走协议行，日志一律 stderr。
   - 方法：`pack.info` / `health` / `judge` / `repair` / `report` / `cancel`。
   - 数据结构：`Verdict{ruleId, status(pass|fail|undetermined|skipped), score, label, evidence, fixed, note}`
     与 `PackInfo{id, name, version, api_version, rules[], capabilities[], deterministic, timeout_secs}`。
   - 错误码：-32001 能力缺失 / -32002 资产不可读 / -32003 参数非法 / -32004 取消 / -32005 超时 / -32000 其他。
   - 语义铁律：超时 → cancel → 杀进程 → 结果记 `undetermined(plugin_timeout)`，绝不冒充 pass/fail。
2. `tools/pack-conformance/`：
   - 假网关（按 manifest capabilities 提供 10 个端点桩）+ 12 条断言；
   - 参考实现假插件（Python，故意正确）+ 4 个故意违规变体（stdout 污染 / 缺方法 / 错误码乱用 / 不响应 health）。

## 验收
1. `python3 tools/pack-conformance/run.py --plugin tools/pack-conformance/fixtures/good` → 12/12 PASS；
2. 对 4 个违规变体逐个跑 → 全部非零退出且**逐条**报出违规点（贴原始输出）；
3. L0 门禁 PASS（本卡无 Rust 改动，仍要贴）。

""" + HONEST,
)

CARDS["K02"] = dict(
    title="【IP-K02】pack-host：manifest 解析 + 子进程生命周期",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K02 pack-host（manifest + process）

## 目标
新建 `crates/pack-host`：manifest 解析与插件子进程生命周期管理。网关端点属 K03，不在本卡。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单（只许改这些）
- `crates/pack-host/Cargo.toml`、`crates/pack-host/src/lib.rs`、`crates/pack-host/src/manifest.rs`、`crates/pack-host/src/process.rs`
- 根 `Cargo.toml`（仅 workspace members 增加 pack-host）
- `crates/pack-host/tests/**`、`crates/pack-host/tests/fixtures/**`
禁止改 svc-pipeline / app-web / core-* / web/。

## 实现要点
1. manifest：`{id,name,version,api_version,entrypoint{cmd,args},capabilities[],deterministic,timeout_secs,permissions{read,net},signature,resources[]}`；
   未知键报错（不静默忽略）；api_version 不兼容 → 明确错误。
2. process：懒启动 → `health` 握手（10s 超时）→ 按 manifest timeout 包每次调用 → 超时先发 `cancel` 再 kill(-TERM → 3s → -KILL)
   → 崩溃自动重启 ×2 → 仍失败进入冷却（300s），期间调用直接返回 `undetermined(plugin_crash)`。
3. 记录：`packId@version + pluginHash`（manifest sha256）随每次调用结果返回。
4. 单测（fixtures 用 K01 的 Python 假插件）：正常握手、健康超时、调用超时、kill -9 后重启、连续崩溃进冷却、manifest 未知键。

## 验收
1. `docker exec -e CARGO_TARGET_DIR=/cargo-target dam-dev bash -lc 'cd <ws> && cargo test -p pack-host'` 全绿（贴测试名清单）；
2. 上面 6 个场景各有一条单测且断言语义正确（崩溃 → undetermined，不是 fail）；
3. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K03"] = dict(
    title="【IP-K03】能力网关：10 端点 + 一次性 Bearer + capabilities 白名单",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K03 能力网关

## 目标
在 pack-host 内实现插件唯一的算力入口：本地 127.0.0.1 随机端口 + 每次启动生成的一次性 Bearer 令牌，
按 manifest `capabilities` 白名单放行。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `crates/pack-host/src/gateway.rs`、`crates/pack-host/src/lib.rs`（仅导出与装配）
- `crates/pack-host/tests/gateway_*.rs`
禁止改其它 crate。

## 端点（复用现有实现，不重写算法）
- `/io/read`
- `/detect/u2net`（engine-segment）、`/detect/ocr`（engine-ocr）
- `/op/crop`、`/op/letterbox`、`/op/upscale`、`/op/replace_bg`、`/op/inpaint`（core-media / 现有 compliance_plugins 实现）
- `/vl/judge`（svc-pipeline::compliance_vl，保留 4096 token 预算与「解析失败落 undetermined」口径）
- `/report/emit`
`letterbox` 为本卡新增：补白让画布尺寸自由，占比与尺寸解耦。

## 验收
1. 集成测试：用 K01 假插件逐个端点打通（每端点至少一条断言，贴测试名）；
2. 越权：manifest 未声明 `op/inpaint` 时调用返回 403，且不产生副作用；
3. 令牌一次性：重启后旧令牌失效；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K04"] = dict(
    title="【IP-K04】官方电商插件（Rust）：六规则 + geometry_solve 闭式求解",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K04 官方电商插件（Rust）

## 目标
新建 `crates/pack-ecommerce`：按协议实现电商六规则判定与修复，修复采用几何闭式求解 + 全量复判。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `crates/pack-ecommerce/**`、根 `Cargo.toml`（workspace members）
禁止改 pack-host / svc-pipeline / app-web / core-* / web/。

## 规则与流程（规则内容来源：packs/ecommerce-taobao/rules.yaml 与 damdesk-repo/references/ecommerce-amazon-pack.md）
- R0 领域 gate（VL，非商品图 → 直接 return，后续不跑）
- R1 尺寸 >=700 / R2 纯白底 / R3 无水印 / R4 主体占比 0.65~0.85 / R5 属性打标（不影响合格结论）
- 修复：
  1) R2 fail → replace_bg；
  2) R4 fail → `geometry_solve`：一次算出 crop → letterbox 到目标占比 → upscale 到短边目标（**必须一次求解，不许逐算子试错**）；
  3) R3 fail → ocr 定位 + inpaint；
  4) 每轮修完调用 `judge` 跑**全量规则**（不是只复检触发规则），未过继续，最多 3 轮，仍不过 → 转人工（undetermined + reason）。
- manifest：`deterministic: true`（仅 VL 规则在 evidence 里标注来源）。

## 验收
1. 对 K00 的 10 张基线图跑 judge：逐图 `ruleId+status+score` 与 `reports/<K00>-baseline.json` 一致（VL 规则按 deterministic 单列）；
2. A4 修复链：至少 4 张原 R4 fail 图，修复产物同时满足 占比>=0.55 + 短边>=1600 + 白底，且复检 pass；
3. 修复轮数 <=3、无死循环（贴每图轮数）；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K05"] = dict(
    title="【IP-K05】执行路径切换：judge/repair 走插件 + DB 0025/0026 + 双跑开关",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K05 执行路径切换

## 目标
把宿主判定/修复执行从「内联 judge_rule/handle_fix + pipeline.yaml 编排」切到「调用插件协议」，
提供 `compliance_engine = legacy | plugin` 双跑开关，默认 legacy；结果落插件来源列。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `crates/svc-pipeline/src/compliance_queue.rs`（执行路径；judge_rule/handle_fix 降级为网关算子实现，逻辑保留）
- `crates/app-web/src/compliance_api.rs`（status 增 pluginState；pack_list 返回内置插件）
- `crates/core-storage/src/migrations/0025_packs_registry.sql`、`0026_verdict_pack_source.sql`
- `crates/core-config/src/`（pipeline.yaml steps/recheck_max 标废弃，不得 panic）
- `web/src/lib/api.ts`（仅类型）
禁止改 `web/src/components/**`、`crates/pack-host/**`、`crates/pack-ecommerce/**`。

## 实现要点
1. `compliance_engine=legacy`：行为与现在完全一致（回归保护）；`plugin`：走 pack-host 调用内置电商插件。
2. DB 0025：`packs(id, version, dir, manifest_hash, api_version, deterministic, signature_status, installed_at, current)`；
   0026：verdict 增 `packVersion`、`pluginHash`。
3. 插件异常映射：timeout → `undetermined(plugin_timeout)`、crash → `undetermined(plugin_crash)`；
   宿主永不因插件失败退出。
4. `pipeline.yaml` 的 steps/recheck_max 读取时不再驱动执行（保持可解析，避免旧包装载炸）。

## 验收
1. legacy 模式对 10 张跑一遍，与 K00 基线逐图一致（回归）；
2. plugin 模式对 10 张跑通（结果可与基线比对，差异需在报告里逐条解释）；
3. 两模式各跑一次 `scripts/journey_acceptance.sh` → PASS；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K06"] = dict(
    title="【IP-K06】测试体系：pack-host 单测 + conformance + 网关 + L1 集成",
    workspace=WS_WT,
    max_runtime="90m",
    skills=["damdesk-testing"],
    body="""# IP-K06 测试体系卡（T1）

## 目标
为插件体系建立容器内可一键跑的测试层，作为 K07 等价验收门的前置。**本卡不改实现**：
发现缺陷用评论反馈到对应实现卡（K02/K03/K04/K05），自己只写测试。

""" + ENV.format(wt=WS_WT) + """
## 覆盖项
1. `cargo test -p pack-host -p pack-ecommerce --workspace`（贴各 suite 的专属 summary 行，不只贴「全绿」）；
2. conformance：`python3 tools/pack-conformance/run.py --plugin <构建出的 ecommerce 插件>` → 12/12；
3. 网关端点集成：10 端点 + 越权 403 + 一次性令牌；
4. 故障注入：kill -9 插件 → undetermined(plugin_crash)；超时 → undetermined(plugin_timeout)；
5. L1：`scripts/journey_acceptance.sh`（流程链路回归）；
6. 覆盖率：`cargo llvm-cov -p pack-host`（若容器无 llvm-cov，改用 `cargo test` 统计 + 在报告里标注「覆盖率未采集」）。

## 验收
1. 上述 6 项逐条 PASS/FAIL 表格（每项贴原始输出）；
2. 新增测试文件清单（`crates/pack-host/tests/*`、`crates/pack-ecommerce/tests/*`）；
3. L0 门禁 PASS；
4. 结论行写明「放行/不放行 K07」及理由。

""" + HONEST,
)

CARDS["K07"] = dict(
    title="【IP-K07】等价验收门禁：四条硬指标（不过即 block，禁止 done）",
    workspace=WS_DIR,
    max_runtime="90m",
    skills=["damdesk-testing"],
    body="""# IP-K07 等价验收门禁

## 目标
插件化后的判定与修复必须与旧引擎等价，且能扛故障。**本卡是门禁：任一条不过就 block，不许 done。**

""" + ENV.format(wt="主仓") + """
## 四条硬指标（全部满足才 done）
1. **等价**：10 张语料逐图 `ruleId+status+score` 与 `reports/<K00>-baseline.json` 一致；
   VL 规则按 `deterministic:false` 单列（允许证据文本不同，status 必须一致）；
2. **故障**：`kill -9` 插件进程 → 任务落 `undetermined(plugin_crash)`，宿主不退出、不冒充 pass/fail；
3. **性能**：单图耗时 <= 旧引擎 ×1.2（同机同语料，贴两组耗时与比值）；
4. **A4 修复链**：修复产物同时满足 占比>=0.55 + 短边>=1600 + 纯白底，且 G6 复检通过。

## 做法
写一个 driver `scripts/pack_equivalence.py`：一次取语料 → 两引擎各跑一遍 → 对比 → 打印 PASS/FAIL 汇总表 → 写 `reports/<task_id>-equivalence.md`。
禁止一条一条手跑命令。

## 验收
1. 报告含四条各自的「命令 + 原始输出 + PASS/FAIL」；
2. 全 PASS → summary 写明四条数字（一致率、崩溃场景、耗时比值、A4 覆盖张数）；
3. 任一条 FAIL → `kanban_block(kind='needs_input')`，reason 写清失败条目 + 根因 + 需决策项，并评论到对应实现卡。

""" + HONEST,
)

CARDS["K08"] = dict(
    title="【IP-K08】.dampack 安装/验签/回滚 + 包管理端点",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K08 安装 / 验签 / 回滚 + 端点

## 目标
插件包可安装、可升级、可回滚、可卸载；安装期做完整校验，坏包明确报错。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `crates/app-web/src/compliance_api.rs`（包端点）
- `crates/pack-host/src/registry.rs`（新增）与 `lib.rs`
- `crates/core-storage/src/migrations/`（如需补充列）
- `web/src/lib/api.ts`（仅类型/方法签名，不改组件）
禁止改 `web/src/components/**`、`crates/svc-pipeline/**`。

## 实现要点
1. `.dampack`（zip）= manifest.json + bin/ + resources/ + README.md；解压到 `{data_dir}/dam/packs/<id>/<version>/`；
2. 校验顺序：sha256 → ed25519 验签（无签名 → 按策略拒绝并给明确错误码）→ `api_version` 兼容 → `capabilities` 齐备 → 写 packs 表；
3. `current` 指针独立存；**回滚 = 切指针**，旧版本目录保留；卸载只允许非内置包；
4. 端点：`POST /packs/install`（上传）、`POST /packs/{id}/activate`、`POST /packs/{id}/rollback`、`DELETE /packs/{id}`、
   `GET /packs`（含版本/运行态/签名状态/是否内置）、`GET /packs/{id}`（manifest + 规则文档，来自 `pack.info`）；
5. 兼容：旧 `packs/import`（YAML 四件）保留一个版本周期，返回「已废弃，请使用 install」，不静默成功。

## 验收
1. 打包 → 安装 → 激活 → 判定 1 张 → 回滚 → 卸载，全链路命令 + 原始返回（贴响应 JSON）；
2. 坏包三例（sha256 不符 / api_version 不兼容 / capabilities 缺项）各自返回明确错误码；
3. 幂等：同一版本重复安装不产生副本目录；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K09"] = dict(
    title="【IP-K09】PackManager 换形态：插件安装/回滚/运行态 + 合规页来源标记",
    workspace=WS_WT,
    max_runtime="2h",
    skills=["damdesk-repo"],
    body="""# IP-K09 PackManager 换形态

## 目标
设置页「行业包」变为「行业插件」管理：安装/升级/回滚/卸载/激活 + 运行态与签名展示；
合规页增加结果来源标记。只改本卡白名单内的前端文件。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单（只许改这些）
- `web/src/components/PackManager.tsx`（296 行 → 约 350 行，同位置 App.tsx:152 不动）
- `web/src/lib/api.ts`（类型 + 方法）
- `web/src/pages/Compliance.tsx`（仅来源标记与 undetermined 文案）
禁止改其它页面/组件（InspectorDock、CompliancePanel、ComplianceReviewDialog 属他人）。

## UI 要求
- 列表列：id / 名称 / 版本 / 来源（内置·安装）/ 运行态（running·冷却·崩溃）/ deterministic / 签名（未签·通过·失败）/ 规则数；
- 行内动作：设为激活、预览（manifest + 规则只读）、回滚、卸载（内置禁用）；
- 底部：安装 `.dampack`（文件选择，替换原「导入目录」）；验签失败/能力声明显式展示；
- 不变：激活唯一性、坏包照列并报错、换插件不删判定历史；
- 删除：「规则可编辑」入口（规则降为只读展示）；
- undetermined 原因新增 `plugin_timeout` / `plugin_crash` 中文文案。

## 验收
1. 新增 RTL 组件测试（`web/src/components/__tests__/PackManager.test.tsx`）：列表渲染、安装调 API、回滚按钮仅在非内置出现、验签失败错误态；
2. `docker exec dam-dev bash -lc 'cd <ws> && pnpm install --prefer-offline && pnpm -C web test && pnpm -C web build'` → PASS；
3. 截图或 DOM 证据（列表 + 安装后行）attach；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K10"] = dict(
    title="【IP-K10】UI 集成验收：安装→激活→判定→回滚→卸载（headless 全流程）",
    workspace=WS_WT,
    max_runtime="90m",
    skills=["damdesk-testing"],
    body="""# IP-K10 UI 集成验收（L1.5）

## 目标
容器内 headless 跑一次完整用户旅程，验证插件管理与判定链在真实界面可用。

""" + ENV.format(wt=WS_WT) + """
## 做法
- 复用 `scripts/cmp_job_e2e.sh` 的起进程 + headless 驱动模式，或写 `scripts/pack_ui_e2e.mjs`；
- 每步截图/DOM 文本落 `reports/<task_id>-ui/`；
- 步骤：打开设置页 → 安装 .dampack → 列表出现新版本 → 设为激活 → 合规页跑 1 张 → 查看结果来源标记 →
  回滚到旧版本 → 卸载 → 列表正确。

## 验收
1. 证据矩阵（步骤 / 命令 / 期望 / 实测 / PASS-FAIL）逐行；
2. 任一步失败：贴控制台错误 + 网络请求原始返回，并评论到 K09/K08；
3. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K11"] = dict(
    title="【IP-K11】Python SDK + 一致性（与 Rust 版 20 张逐图对齐）",
    workspace=WS_WT,
    max_runtime="90m",
    skills=["damdesk-repo"],
    body="""# IP-K11 Python SDK + 一致性

## 目标
让合作伙伴能用 Python 写行业插件：SDK + 样例电商包，并与 Rust 版判定结果一致。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `tools/pack-sdk-python/**`、`tools/pack-conformance/**`（仅新增 Python 相关断言）
禁止改 Rust crate 与 web/。

## 要求
1. `gateway.py`：stdio JSON-RPC 客户端 + 类型 + 装饰器注册；
   必守两个坑：`proto_out = os.fdopen(os.dup(1)); os.dup2(2,1)`（防库 print 污染协议）；
   输出 utf-8 + `ensure_ascii=False` + 逐行 flush（Windows 显式 `newline="\\n"`）。
2. 样例电商包（`examples/ecommerce_py/`）：调用同一批网关端点（u2net/crop/letterbox/upscale/replace_bg/inpaint/vl_judge）。
3. 与 Rust 版对齐：10 张语料逐图 `ruleId+status+score` 一致（score 容差 1e-6，VL 规则单列）。

## 验收
1. `python3 tools/pack-conformance/run.py --plugin tools/pack-sdk-python/examples/ecommerce_py` → 12/12；
2. 一致性报告（10 张，逐图对比表）；
3. 启动时间与体积记录（供文档用）；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K12"] = dict(
    title="【IP-K12】房产插件 + 宿主零改动验收（git diff = 0）",
    workspace=WS_WT,
    max_runtime="90m",
    skills=["damdesk-repo"],
    body="""# IP-K12 房产插件 + 宿主零改动

## 目标
用第二个行业验证「新增行业插件时宿主零改动」这条架构承诺。

""" + ENV.format(wt=WS_WT) + """
## 文件白名单
- `crates/pack-realestate/**`、根 `Cargo.toml`（workspace members）
禁止改 pack-host / app-web / svc-pipeline / core-* / web/（这批文件的任何改动都会让本卡验收失败）。

## 规则（来源 packs/realestate + damdesk-repo/references）
- 仅几何修复（crop/letterbox/upscale），**禁生成式**；
- 判定失败且无法几何修复 → 强制转人工（undetermined + 原因），不自动放行；
- manifest：`deterministic: true`。

## 验收
1. `git -C <repo> diff --stat main -- crates/pack-host crates/app-web crates/svc-pipeline crates/core-* web | tail -1` → 空输出（宿主零改动，贴原始命令与输出）；
2. 房产 4 条规则对 10 张房产语料实跑，结果矩阵（含「转人工」项）；
3. conformance 12/12；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K13"] = dict(
    title="【IP-K13】lama 模型下载/内置（inpaint 真机可用）",
    workspace=WS_WT,
    max_runtime="90m",
    skills=["damdesk-repo"],
    body="""# IP-K13 lama 模型下载/内置

## 目标
补齐已知产品缺口：`model_store` 有 lama manifest 但无任何下载入口，安装包 resources 里没有 lama，
真机 A5 inpaint 必然 `ModelMissing` 转人工。

""" + ENV.format(wt=WS_WT) + """
## 事实（已实测，勿重复侦察）
- `crates/app-tauri/resources/models/` 只有 siglip2 / dinov3 / ppocrv6-det / rec / realesr-general-x4v3 / u2net；
- `model_store` 有 lama manifest，无下载入口；
- lama-fp32.onnx 约 208MB。

## 交付（二选一，报告写清选了哪个 + 理由）
A. 首启下载：model_store 增加下载入口（进度/失败重试/校验 sha256），设置页可见；
B. 随包内置：resources 增 lama，安装包体积 +208MB。
硬要求：不论哪条，`/op/inpaint` 在模型可用时不再返回 ModelMissing。

## 验收
1. 模型就绪检测命令 + 原始输出；
2. 对 2 张有水印语料跑 A5 修复 → 不再是 ModelMissing（成功或明确的其它失败原因）；
3. 失败路径可诊断（下载失败/校验失败各有明确错误）；
4. L0 门禁 PASS。

""" + HONEST,
)

CARDS["K14"] = dict(
    title="【IP-K14】出包（只出包，不装机）：合并分支 + L0 + NSIS",
    workspace=WS_DIR,
    max_runtime="2h",
    skills=["damdesk-testing"],
    body="""# IP-K14 出包（只出包）

## 目标
产出可装机的 Windows 安装包。**只出包：禁止 scp / 装机 / 写真机报告**（真机清单移交 K15）。

""" + ENV.format(wt="主仓") + """
## 步骤（顺序固定）
0. **先合并全部分支**：`git -C /data/projects/soujpg-dam-desktop merge --no-edit wt/<K01..K13 各分支>`；
   octopus 冲突时退化顺序合并，解冲突保留三侧全部改动；合并后 `git log --oneline -5` 贴证据。
   禁止「不合并直接构建」——那样打出的包不含任何修复，且门禁照样绿（假成功）。
1. 合并后跑全量 L0 + `scripts/journey_acceptance.sh` → PASS；
2. 出包容器 `dam-chain-win`（Windows 交叉编译镜像）：
   `pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis`（20–30min，后台跑，不前台 sleep）；
3. 产物在 `/cargo-target/x86_64-pc-windows-msvc/release/bundle/nsis/*.exe`，`docker cp` 出来，
   命名 `souJpg-DAM-<版本>-<commit>_x64-setup.exe`，放 `/data1/dam-models/releases/`；
4. 核 web dist 含本链修复：`docker exec dam-chain-win bash -lc "cd /workspace && grep -c 'packId\\|pluginHash' web/dist/assets/*.js"`；
5. 核 `git ls-remote gitea main` 与本地 HEAD 一致。

## 验收
1. 包绝对路径 + sha256 + 体积 + 对应 commit + dist marker 五项齐（写 summary + metadata）；
2. 合并证据（`git log --oneline -5` 与各分支已进 main 的 `merge-base --is-ancestor` 输出）；
3. L0/L1 PASS；
4. **未装机**（明确写「未在真机安装，移交 K15」）。

""" + HONEST,
)

CARDS["K15"] = dict(
    title="【IP-K15】真机 E2E（唯一真机入口）：Windows 装包→装插件→判定→回滚→卸载",
    workspace=WS_DIR,
    max_runtime="3h",
    skills=["damdesk-testing"],
    body="""# IP-K15 真机 E2E（唯一真机入口）

## 目标
在 Windows 真机上把插件管理 + 判定链走一遍。**本卡是唯一允许碰真机的卡**；
装一次机跑全部项，禁止逐项重复装机。

## 环境
- 真机：Lenovo Windows，`Administrator@172.25.45.49`（密码 139jasstion），sshpass 可用；
- 安装包：K14 产物（读 K14 summary/metadata 的路径 + sha256，先核 sha256 再装）；
- 目标机日志/数据目录：`%APPDATA%\\com.soujpg.dam.desktop`。

""" + ENV.format(wt="主仓")
    + """
## 步骤（一次装机跑完）
1. 核包：sha256 与 K14 记录一致；
2. 安装 → 启动 → 首次启动无崩溃（截日志关键行）；
3. 设置页 → 行业插件：安装 .dampack（官方电商插件）→ 列表出现 → 设为激活；
4. 导入 2 张测试图 → 跑判定 → 查看结果（来源标记 packId@version、undetermined 文案）；
5. 回滚到上一版本 → 卸载 → 列表/运行态正确；
6. 故障注入（可选）：任务管理器杀掉插件进程 → 判定落 undetermined(plugin_crash)，应用不崩。

## 验收
1. 证据矩阵（项 / 操作 / 期望 / 实测 / PASS-FAIL）逐行，含截图或日志片段；
2. 任何 FAIL 给原始日志 + 判因（代码缺陷 / 平台差异 / 模型缺失 / 环境问题），不许混为一谈；
3. 装一次机的证明：报告里贴安装时间戳与「未重复安装」说明；
4. 明确写「哪些项未验证」。

""" + HONEST,
)

# 父关系独立维护（漏写即抛错，不静默建无父卡）
PARENTS = {
    "K00": [],
    "K01": ["K00"],
    "K02": ["K01"],
    "K03": ["K02"],
    "K04": ["K03"],
    "K05": ["K04"],
    "K06": ["K05"],
    "K07": ["K06"],
    "K08": ["K07"],
    "K09": ["K08"],
    "K10": ["K09"],
    "K11": ["K07"],
    "K12": ["K10", "K11"],
    "K13": ["K07"],
    "K14": ["K10", "K12", "K13"],
    "K15": ["K14"],
}

ORDER = ["K00", "K01", "K02", "K03", "K04", "K05", "K06", "K07",
         "K08", "K09", "K10", "K11", "K13", "K12", "K14", "K15"]


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def create_card(code, parents_ids, dry_run=False):
    c = CARDS[code]
    body = OVERRIDES.get(code, "") + c["body"]
    if dry_run:
        print(f"[dry-run] {code} parents={parents_ids} ws={c['workspace']} runtime={c['max_runtime']}")
        print(f"    title: {c['title']}")
        return f"<{code}>"
    fd, path = tempfile.mkstemp(prefix=f"kb_{code}_", suffix=".md")
    with os.fdopen(fd, "w") as f:
        f.write(body)
    cmd = ["hermes", "kanban", "--board", BOARD, "create", c["title"],
           "--assignee", PROFILE, "--body-file", path, "--workspace", c["workspace"],
           "--max-retries", "4", "--max-runtime", c["max_runtime"],
           "--model", MODEL, "--provider", PROVIDER,
           "--idempotency-key", f"industry-plugin-{code}", "--json"]
    if c["workspace"].startswith("worktree"):
        cmd += ["--branch", f"wt/industry-{code.lower()}"]
    for p in parents_ids:
        cmd += ["--parent", p]
    for s in c.get("skills", []):
        cmd += ["--skill", s]
    r = run(cmd)
    os.unlink(path)
    if r.returncode != 0:
        print(f"FAIL {code}: exit={r.returncode}\nSTDOUT={r.stdout}\nSTDERR={r.stderr}")
        sys.exit(1)
    try:
        data = json.loads(r.stdout.strip())
        tid = data.get("id") or data.get("task", {}).get("id")
    except Exception as e:
        print(f"FAIL {code}: --json 解析失败 ({e})\nSTDOUT={r.stdout[:2000]}")
        sys.exit(1)
    if not tid:
        print(f"FAIL {code}: 未解析到 id: {r.stdout[:500]}")
        sys.exit(1)
    print(f"OK   {code} -> {tid}")
    return tid


def reconcile(ids):
    """建完对账：链式卡不得在 ready；task_links 与 PARENTS 逐边比对。"""
    con = sqlite3.connect(BOARD_DB)
    cur = con.cursor()
    by_id = dict(ids.items().__iter__())  # code -> tid
    tid2code = {v: k for k, v in by_id.items()}
    errs = []

    # 1) 状态：有父卡的卡必须是 todo
    for code, tid in by_id.items():
        row = cur.execute("select status from tasks where id=?", (tid,)).fetchone()
        status = row[0] if row else "MISSING"
        want = "todo" if PARENTS[code] else "ready"
        if status not in (want, "blocked", "scheduled"):
            errs.append(f"{code} {tid} status={status}, 期望 {want}")

    # 2) 依赖边逐边比对
    actual = set(cur.execute("select parent_id, child_id from task_links").fetchall())
    expected = set()
    for code, ps in PARENTS.items():
        for p in ps:
            if p in by_id and code in by_id:
                expected.add((by_id[p], by_id[code]))
    missing = expected - actual
    extra = {e for e in actual if e[1] in tid2code} - expected
    for m in missing:
        errs.append(f"缺边 {tid2code.get(m[0], m[0])} -> {tid2code.get(m[1], m[1])}")
    for e in extra:
        errs.append(f"多边 {tid2code.get(e[0], e[0])} -> {tid2code.get(e[1], e[1])}")

    con.close()
    if errs:
        print("\n对账失败：")
        for e in errs:
            print("  -", e)
        sys.exit(2)
    print(f"\n对账通过：{len(by_id)} 张卡，{len(expected)} 条依赖边与设计一致，链式卡均为 todo。")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--only", help="只建这一张（父卡必须已建，用 --parents-map 显式给父 id）")
    ap.add_argument("--parents-map", help='JSON: {"K06":"t_xxx","K07":"t_yyy"} 供 --only 用')
    args = ap.parse_args()

    if args.only:
        if args.only not in CARDS:
            print(f"未知卡号 {args.only}；可用：{sorted(CARDS)}")
            sys.exit(1)
        pm = json.loads(args.parents_map) if args.parents_map else {}
        missing = [p for p in PARENTS[args.only] if p not in pm]
        if missing:
            print(f"{args.only} 的父卡未提供：{missing}")
            sys.exit(1)
        ids = {**pm, args.only: None}
        tid = create_card(args.only, [pm[p] for p in PARENTS[args.only]], args.dry_run)
        print(f"{args.only} -> {tid}")
        return

    if args.dry_run:
        total_edges = 0
        for code in ORDER:
            # 检查父关系完整性
            for p in PARENTS[code]:
                assert p in CARDS, f"{code} 的父卡 {p} 不存在"
                total_edges += 1
        for code in ORDER:
            create_card(code, [f"<{p}>" for p in PARENTS[code]], dry_run=True)
        print(f"\n共 {len(ORDER)} 张卡 / {total_edges} 条依赖边（dry-run，未建卡）")
        return

    ids = {}
    for code in ORDER:
        parents_ids = [ids[p] for p in PARENTS[code]]
        ids[code] = create_card(code, parents_ids)

    # 思考深度：CLI 无参数，建完直接写板库（列名 reasoning_effort）
    con = sqlite3.connect(BOARD_DB)
    con.executemany("update tasks set reasoning_effort=? where id=?",
                    [(REASONING, t) for t in ids.values()])
    con.commit()
    rows = con.execute(
        f"select id, model_override, provider_override, reasoning_effort from tasks where id in ({','.join('?'*len(ids))})",
        list(ids.values())).fetchall()
    con.close()
    bad = [r for r in rows if r[1] != MODEL or r[2] != PROVIDER or r[3] != REASONING]
    print(f"模型/深度核验：{len(rows)-len(bad)}/{len(rows)} 张 = model={MODEL} provider={PROVIDER} reasoning={REASONING}")
    if bad:
        print("不一致：", bad); sys.exit(3)

    reconcile(ids)


if __name__ == "__main__":
    main()
