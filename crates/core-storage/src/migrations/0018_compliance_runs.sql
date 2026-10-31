-- 0018 compliance_runs — CMP-JOB-01（卡 task）。
--
-- 用户执行**一次批量图片审核 = 一个独立 job（run）**。job 跑完就给这个 job 出结果统计
-- （成功/失败/跳过数等），**各 job 之间必须明显分开**，否则用户看到全局累计的失败数会以为
-- 系统一直在坏。job 内部：每个处理步骤是一个 task（`job_queue` 的一行）。
--
-- 为什么必须落表而不是只放内存：
-- 1. `state.compliance.report` 是进程内单槽，进程重启即丢，且没有历史 —— 用户需要「上一次、
--    上上次」各自的结果；
-- 2. `job_queue` 原先**没有 run 维度**（每行只有 kind/targetId/payload/status/attempts），
--    所以 `compliance_stats` 天生是「历史累计」：BATCH-01 造的坏图让 assetFailed=30 永久
--    挂在那儿，与「本轮 failed=0」并排显示必被误读。
--
-- 口径（写死在这里，后续别再各算一套）：
-- * 一次判定 = 一行 `compliance_runs`，其下 task 全部带同一个 `job_queue.runId`；
-- * 迁移前的老行 `runId` **保持 NULL**（归入「迁移前」伪 run），**不回填**：
--   回填会让历史 job 在新 run 的统计里复活；
-- * 计数器语义：`total` = 本轮任务数；`done` = 成功（**不含 skipped**）；`failed` = 终态失败；
--   `skipped` = 跳过（文件不存在/非图片/已删/失联 —— job_queue 里与成功同为 done 终态，
--   故单独计数）；`cancelled` = 取消后**未执行**的任务数（仍留在队列里，可下次续跑）。
--   恒等式：total = done + failed + skipped + cancelled；
-- * 状态：running / done（正常收工，即使有 task 失败也仍是 done）/ cancelled（用户取消）/
--   interrupted（进程重启或崩溃，未收工）/ failed（引擎级错误，队列没跑起来）。
--
-- 迁移纪律：只加不改不删。`job_queue` 只加一列。

CREATE TABLE compliance_runs (
    runId       INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 目前恒为 'compliance'（预留：其它批量作业若要复用这套「一次执行一个 run」的语义）
    kind        TEXT    NOT NULL DEFAULT 'compliance',
    status      TEXT    NOT NULL DEFAULT 'running',
    startedAt   INTEGER NOT NULL DEFAULT (unixepoch()),
    -- NULL = 还在跑（或进程死在中间 —— 由启动自愈改成 interrupted 并落戳）
    finishedAt  INTEGER,
    total       INTEGER NOT NULL DEFAULT 0,
    done        INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    skipped     INTEGER NOT NULL DEFAULT 0,
    cancelled   INTEGER NOT NULL DEFAULT 0,
    -- 引擎级错误/收工说明（正常收工为 NULL）
    error       TEXT,
    -- 本次执行的入参快照（scope / packId / limit / workers / 候选范围），界面与复盘用
    params      TEXT
);

-- 老库跑两次不报错：建表用 IF NOT EXISTS（列级 ALTER 由迁移账本保证只跑一次）
CREATE INDEX IF NOT EXISTS idx_compliance_runs_status
    ON compliance_runs(status, startedAt);

ALTER TABLE job_queue ADD COLUMN runId INTEGER;

-- 按 run 取 task 明细 / 汇总计数（job 列表点开就是这条路径）
CREATE INDEX IF NOT EXISTS idx_job_queue_run ON job_queue(kind, runId);
