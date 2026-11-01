# DECouple-BE：判定数据下沉 job 账本 + human_review + 存量迁移 + 资产 API 判定字段下线

卡：`t_59942f03`（用户裁决 2026-10-31：图库域与 job 域完全解耦）。基线 `gitea/main d944511`，分支 `wt/t_59942f03`。

## 现状（实测，非推测）

| 事实 | 位置 |
|---|---|
| assets 表**没有任何判定列**（无 finalStatus / assetHumanReview） | `migrations/0001_init.sql` + 全部 `ALTER TABLE assets` |
| 人审账本是 `verdict_human_review`，键 = `(packId, assetId, ruleId)`，**无内容版本** ⇒ 资产修图/重导后旧人审仍生效 | `migrations/0017_human_review.sql` |
| 资产级「生效结论」是查询时合成的（`state_of_verdicts_with`），并不落库 | `svc-library/src/compliance_view.rs` |
| 资产级 API 暴露 `finalStatus` / `assetHumanReview` / `state` / `machineState` / `machineConfidence` | `app-web/src/compliance_api.rs::asset_verdict_payload`（`/api/compliance/verdicts`） |
| 图库列表接受判定维度过滤 `?compliance=`（判定概念漏到图库域） | `app-web/src/library_api.rs` |
| F8 图库导出**已经**不写判定报告（`report: None`，注释「HTTP 层 v1 暂不透传」） | `app-web/src/export_api.rs:152` |
| 资产内容 hash 两处可挂：`assets.fileHash`（导入 sha256）+ `asset_versions.sha256`（`current=1` 的工作版本） | `0001` / `0032_asset_versions.sql` |

## 设计（本卡落地口径）

1. **人审新表 `human_review`**（migration 0035，`只加不改`）：
   `(assetId, schemeId, ruleId|NULL, humanStatus, humanReason, judgedAt, reviewedBy, contentVersion, machineResult, machineConfidence, revokedAt, revokeReason)`。
   - 唯一性：`(assetId, schemeId, COALESCE(ruleId,'*')) WHERE revokedAt IS NULL`（部分唯一索引，与 0017 同款）。
   - `schemeId` = 判定方案身份。当前判定账本只记 `packId`（`rule_verdict` 无 spec 列），故落库值 = `pack:<packId>`；
     将来规格化可直接用 `spec:<id>` 前缀，**跨 job 生效**语义不变（人审从不绑 run/job）。
   - `contentVersion` = 写入时的资产内容版本 = `asset_versions(surrent=1).sha256 ?? assets.fileHash ?? ''`。
     读取时**只认与当前内容版本相等的活记录** ⇒ 修图产生新工作版本、重导换内容，人审自动失效（不删行，可审计）。
2. **存量迁移**：0035 内 `INSERT OR IGNORE` 把 `verdict_human_review` 活/历史行搬进新表（幂等）；
   独立脚本 `scripts/migrate_human_review_v2.py` 负责生产纪律（全量备份 + sha256 → 幂等搬 → 回读校验 100% → purge 旧表），`--dry-run` 可先看。
3. **资产 API 下线判定字段**：`/api/compliance/verdicts` 不再返回资产级合成结论（`finalStatus` / `assetHumanReview` /
   `state` / `machineState` / `machineConfidence`）；未给 `runId` 时只回**判定行原文**。
   给了 `runId` 时按「这一次执行的 job 账本 + human_review」**查询时合成** `machineStatus` / `effectiveStatus` / `override`。
4. **「当前生效结论」不落资产**：新增 `GET /api/compliance/jobs/{runId}/verdicts`（L3 下钻），逐资产给
   机读结论 + 人审覆盖 + 合成生效结论，全部查询时算；旧名 `finalStatus` 全仓消失（改名 `effectiveStatus`）。
5. **图库域不再读判定**：`/api/library/assets` 的 `?compliance=` 维度下线（400 `compliance_filter_moved`，指路 job 端点）；
   `ImportFilter.compliance` 只留给 job 侧候选选择/导出报告复用，不再经图库 API 暴露。
6. **job 结果导出**：`POST /api/compliance/jobs/{runId}/export`（过的一批 / 未过的一批 / 任选档位），
   复用 `svc_library::export::run_export`（选择集 = 该 run 的资产按合成生效结论过滤），与 F8 图库导出零耦合。

## 实施顺序

迁移 → core-storage（struct/trait/impl）→ svc-library（合成/失效）→ app-web（verdicts / review / job export / library）→
单测 + 集成测试 → 迁移脚本 → L0 门禁。

## 验收口径

- `/api/library/assets` 原始响应无任何判定字段，`?compliance=` 400；
- `/api/compliance/verdicts` 无 `finalStatus`/`assetHumanReview`；给 `runId` 才有合成结论；
- 人审失效单测：写入人审 → 改 `assets.fileHash` / 插 `current=1` 工作版本 → 旧人审不再生效（`effectiveStatus` 回落到机器结论），
  新内容上重新改判 → 再次生效；
- job 导出：只导出该 run 命中档位的资产，落盘文件数 == 命中数，manifest 齐全；
- 迁移脚本 `--dry-run` + 实跑：迁移前后行数、回读校验 100%、purge 后旧表不存在；
- `bash scripts/test_gate.sh` 全绿。
