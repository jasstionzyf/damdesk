# 行业插件迁移 —— kanban 链拆解与创建 Plan

依 `kanban-orchestration`（default profile 权威）+ `references/chain-soujpg.md` 环境配方。落地依据：`2026-10-20_industry-plugin-migration-plan.md`。

## 1. 链形状（16 卡，soujpg 板）

```
K00 基线(ENV+BASELINE)
 └ K01 协议 + conformance 骨架
    └ K02 pack-host（manifest/process）
       └ K03 能力网关（10 端点 + Bearer + capabilities）
          └ K04 电商 Rust 插件（六规则 + geometry_solve）
             └ K05 执行路径切换 + DB 0025/0026 + 双跑开关
                └ K06 测试体系卡（T1：单测+conformance+L1）
                   └ K07 等价验收（门禁卡，四条硬指标）
                      ├ K08 .dampack 安装/签名/回滚 + 端点
                      │  └ K09 PackManager 换形态 + 合规页来源标记
                      │     └ K10 UI 集成验收（L1.5 headless 全流程）
                      ├ K11 Python SDK + conformance（并行支）
                      └ K13 lama 模型内置（并行支）
K12 房产插件 + 宿主零改动  ← parents = [K10, K11]
K14 出包（只出包，NSIS）  ← parents = [K10, K12, K13]
K15 真机 E2E（唯一真机入口）← parents = [K14]
```

并行支判定：K11（Python SDK，只动 `tools/`）、K13（模型下载，只动 `model_store`/resources）与 K08–K10 文件面不相交，放 K07 之后并行；同 profile 实际会被 dispatcher 串行执行，但依赖边不虚设。

## 2. 卡表

| 卡 | 类型 | 文件面（白名单） | max-runtime | 验收要点 |
|---|---|---|---|---|
| K00 基线 | ENV-PREP | 无（只读 + `scripts/pack_baseline_capture.py`） | 30m | 工作树认领干净；dam-dev 容器 + cargo/pnpm 可用；**20 张语料旧引擎基线 JSON**（ruleId/status/score）落 `reports/` |
| K01 协议 | 规格 | `docs/`, `tools/pack-conformance/` | 60m | protocol.md；conformance 对违规假插件逐条报错、对参考实现全绿 |
| K02 pack-host | 实现 | `crates/pack-host/{manifest,process,runtime}.rs`, `tools/dampack_launcher.py`, workspace Cargo.toml | 2h | 懒启动/health/超时/崩溃重启×2/冷却；共享 runtime（仅解释器）+ launcher（stdout 保护 / add_dll_directory / import 失败原因）单测全覆盖 |
| K03 网关 | 实现 | `crates/pack-host/src/gateway.rs`, `lib.rs` | 2h | 10 端点复用现有 crate；Bearer 一次性；capabilities 越权 403 |
| K04 电商包 | 实现 | `packs/plugins/ecommerce/**`（Python 源码，manifest + plugin + requirements） | 2h | 10 张与 K00 基线逐图一致；A4 修复链三规则同过；geometry_solve 一次求解 |
| K05 切路径 | 实现 | `svc-pipeline/compliance_queue.rs`, `app-web/compliance_api.rs`, `core-storage/migrations/`, `core-config/`, `web/src/lib/api.ts` | 2h | legacy 回归不变；plugin 模式跑通；双跑开关可用 |
| K06 测试 | 自验 | `tests/`, `scripts/` | 90m | L0 + L1 journey + conformance 全绿；覆盖率报告；**不改实现** |
| K07 等价门禁 | 门禁 | `scripts/`, `reports/` | 90m | 四条硬指标；**不过 → block(needs_input) + 报告，禁止 done** |
| K08 安装/venv/签名 | 实现 | `app-web(包端点)`, `pack-host(registry)`, migrations | 2h | 装（含建 venv + pip install）→激活→回滚→卸载；未签名按策略拒；同版本幂等 |
| K09 PackManager | 实现(前端) | `web/src/components/PackManager.tsx`, `web/src/lib/api.ts`, `web/src/pages/Compliance.tsx` | 2h | 列表新字段 + 安装/回滚/卸载；RTL 组件测试；`pnpm -C web test` PASS |
| K10 UI 集成 | 验收 | `scripts/`（headless driver） | 90m | 设置页装包→激活→判定→回滚→卸载，证据矩阵 |
| K11 Python SDK | 实现 | `tools/pack-sdk-python/`, `tools/pack-conformance/` | 90m | **官方主路径**：官方包即用该 SDK；conformance 全绿；stdout 保护/venv 依赖声明 |
| K12 房产包 | 实现 | `packs/plugins/realestate/**`（Python 源码） | 90m | **宿主 crate + web 改动 = 0**（`git diff --stat` 证据）；仅几何、禁生成式 |
| K13 lama 模型 | 实现 | `model_store` 下载入口 + resources | 90m | A5 inpaint 不再 ModelMissing |
| K14 出包 | 收口 | `dam-chain-win` 构建 + 合并分支 | 2h | 先合并分支→L0 全量→NSIS + sha256 + commit + dist marker；体积预算（runtime 一份 + 每包 <2MB）；**禁装机** |
| K15 真机 E2E | E2E | Lenovo 172.25.45.49 | 3h | 唯一真机入口；装包→装插件→激活→判定→回滚→卸载，证据矩阵；**发现问题：先记 `damdesk-repo/references/known-issues.md`，再自动建修复卡（parent=K15，验收=代码+单测），阻塞项才 block** |

## 3. 建卡与对账（脚本）

```bash
# 预览（不建卡）
python3 .hermes/plans/create_industry_plugin_chain.py --dry-run
# 建卡（幂等：--idempotency-key industry-plugin-<code>）
python3 .hermes/plans/create_industry_plugin_chain.py
# 只建某段（含其父依赖已建）
python3 .hermes/plans/create_industry_plugin_chain.py --only K07
```

脚本按 `kanban-orchestration` 硬规则：

- 卡规格与父关系**分开维护**（`CARDS` / `PARENTS` 两张表），父关系缺失即抛错，不静默建无父卡；
- 拓扑序创建，`--parent` 可重复、create 时一次给全（带 parent 的卡初始 `todo`，不会被 dispatcher 秒拉）；
- 长正文一律 `--body-file`，`--json` 解析**整个 stdout**（pretty-print 多行）；
- 建完自动对账：① `list --status ready` 中不得出现链式卡；② 板库 `task_links` 与 `PARENTS` 逐边比对；差异非零退出；
- `--max-retries 4`（= 3 次重试）、`--max-runtime` 按卡、`--workspace worktree:/data/projects/soujpg-dam-desktop`；K00/K14/K15 用 `dir:`（跑主干/出包/真机）。

## 4. 运行期纪律（写进每张卡体）

- **L0 每卡必跑**：`docker exec -e CARGO_TARGET_DIR=/cargo-target dam-dev bash -lc 'cd <ws> && bash scripts/test_gate.sh'` → 卡里贴 `L0 门禁：PASS <耗时>`。
- **L1** 仅流程相关卡（K05/K06/K08）跑 `scripts/journey_acceptance.sh`；**L1.5**（`cmp_job_e2e.sh`）只在 K10；**L2 真机**只在 K15。
- **环境事实**：宿主 `/data/projects/soujpg-dam-desktop` ↔ 容器 `dam-dev:/workspace`；worktree 在 `.worktrees/<task_id>`（容器 `/workspace/.worktrees/<id>`）；新 worktree 先 `pnpm install --prefer-offline`；宿主无 node/pnpm。
- **报告落宿主** `reports/<task_id>-*.md`，不写进 worktree。
- **禁止** `git add -A`；只提交自己白名单内文件；内网 push 用 `git -c http.proxy= push`。
- **诚实门**：未实测的项必须标「未验证」，不得写成通过。
- 门禁卡（K07）失败时 `kanban_block(kind=needs_input)` 附结构化报告（结论/已尝试/根因/需决策/可复现/产物）。

## 5. 与历史卡的关系

- **链外先跑**：`t_d190732b ECOM-PACK-4-WIN-VERIFY`（已授权）先按原口径跑一次作旧引擎 Windows 基线，不并入本链；跑完归 done。
- ECOM-PACK-1 的 20 张语料 = K00 基线 + K04/K07 验收语料，不重采。
- C0–C9 / A 系列已 done 卡全部沿用（见迁移 plan §6.1），封存卡（C2/C3/C4/C7/ECOM-PACK-2/2B/3）不重启。

## 6. 风险与回滚

- K07 不过 → 链冻在门禁，K08 起全部 `todo` 等待，不会误派；修复卡插入 = 新建 `parents=[K05]` 的修复卡 + `link` 到 K06/K07。
- 双跑开关 `compliance_engine=legacy|plugin`：K05 之后任何阶段可一键切回旧引擎。
- K14 出包前必须合并全部分支（`git merge --no-edit wt/<各卡>`），否则打出的是基线包（假成功里最难发现的一种）。
- 真机资源独占：K15 前禁止任何卡碰 Lenovo 机器；K15 卡体写死「装一次机跑全部项，禁止逐项重复装机」。

