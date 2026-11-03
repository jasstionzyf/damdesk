# ARCHIVE — 验收报告与过程产物索引

验收报告、证据、一次性脚本不在本仓库。历史产物按卡归档在 kanban 工件目录：

- 按卡：`~/.hermes/kanban/artifacts/soujpg/<task_id>/`（报告 + evidence 子目录）
- 无卡号历史件：`~/.hermes/kanban/artifacts/soujpg/_misc/`
- 一次性脚本存档：`~/.hermes/kanban/artifacts/soujpg/_scripts/`

`reports/`、`artifacts/` 已加入 `.gitignore`；本仓只保留代码与工程脚本
（`scripts/` 白名单见 `scripts/publish_public.sh` 的 SYNC_SCRIPTS 及仓内长期维护项）。

实施计划（`PLAN.md`）与 plan 稿（`.hermes/plans/`）属内部文档，只在内部主线维护，不入公开仓；
代码注释里出现的 `PLAN.md §x` 引用指向内部文档。
