//! CMP-JOB-01 数据访问：**执行（run）**维度的合规批量作业账本（存储解耦裁决：trait 与 SQL 分离）。
//!
//! 语义（用户 2026-10-18 裁决，权威）：
//! - **一次批量审核 = 一个 run**（`compliance_runs` 一行），run 内部每个处理步骤是一条 task
//!   （`job_queue` 一行，带同一个 `runId`）。run 跑完要能回答「这一次成功多少、失败多少、
//!   跳过多少、耗时多少」，而不是把全库历史累计数糊在一起；
//! - `job_queue.status` 只有 pending/running/done/failed 四态，**跳过与成功同为 done**
//!   （跳过是「这条没东西可判」，不是失败）。所以 `skipped` 单独计数落在 run 行上，其余
//!   计数一律由 `job_queue WHERE runId = ?` 汇总 —— 单一事实源，不另建影子表；
//! - 迁移前的老任务 `runId = NULL`，**归入「迁移前」伪 run，永不回填**：回填会让历史失败数
//!   在新 run 的统计里复活（这正是 CMP-JOB-01 要治的病）。
//!
//! 复用边界：本模块只管「run 账本 + 挂在 run 上的 task 视图」，不碰判定口径
//! （判定结论在 `rule_verdict`，见 [`crate::compliance`]）。

use serde::{Deserialize, Serialize};

use crate::Result;

/// run 状态词表（migration 0018 注释为准）。
pub mod run_status {
    /// 正在跑（进程活着；进程死了由启动自愈改成 [`INTERRUPTED`]）
    pub const RUNNING: &str = "running";
    /// 正常收工 —— **即使有 task 失败也仍是 done**（done 只表示「这次执行跑完了」）
    pub const DONE: &str = "done";
    /// 用户取消：未执行的任务留在队列里（pending，可下次续跑）
    pub const CANCELLED: &str = "cancelled";
    /// 进程重启/崩溃导致没收工（启动自愈时判定，不是用户动作）
    pub const INTERRUPTED: &str = "interrupted";
    /// 引擎级错误：队列根本没跑起来（入队失败、通道中断等）
    pub const FAILED: &str = "failed";
}

/// run 行（`compliance_runs`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRow {
    pub run_id: i64,
    pub kind: String,
    pub status: String,
    pub started_at: i64,
    /// None = 未收工（running / 进程死在中间且尚未自愈）
    pub finished_at: Option<i64>,
    pub total: i64,
    /// 成功（**不含 skipped**）
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    /// 取消/中断后未执行的任务数（= total − done − failed − skipped）
    pub cancelled: i64,
    pub error: Option<String>,
    /// 入参快照（json）
    pub params: Option<String>,
}

impl RunRow {
    /// 耗时（毫秒）。未收工 → None（别拿 now 冒充已完成的耗时）。
    pub fn duration_ms(&self) -> Option<i64> {
        self.finished_at
            .map(|f| (f - self.started_at).max(0) * 1000)
    }

    pub fn is_running(&self) -> bool {
        self.status == run_status::RUNNING
    }
}

/// 挂在某个 run 上的 task（= `job_queue` 一行）。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub job_id: i64,
    pub run_id: Option<i64>,
    pub kind: String,
    pub target_id: i64,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub updated_at: i64,
    /// 原始 payload（json；界面用它还原「哪张图/哪个组/哪些规则」）
    pub payload: Option<String>,
}

/// run 内任务计数（`job_queue WHERE runId = ?` 汇总）。
///
/// `done` 是**行数**（含 skipped，因为两者终态都是 done）；用 [`RunCounts::with_skipped`]
/// 把 run 行上的 skipped 扣出来，才是用户看到的「成功数」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCounts {
    pub total: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    /// pending + running
    pub pending: i64,
}

impl RunCounts {
    /// 扣掉 skipped：done = done行数 − skipped（跳到 0 为止，绝不出现负数）。
    pub fn with_skipped(mut self, skipped: i64) -> Self {
        let skipped = skipped.clamp(0, self.done);
        self.skipped = skipped;
        self.done = (self.done - skipped).max(0);
        self
    }

    /// 未结算（pending + 执行中）的任务数 —— **队列口径**。
    ///
    /// 除收工记账（[`ComplianceRunRepo::finish`]）外，任何对外视图都该用这个名字：
    /// 运行中的执行里这些任务既没取消也没失败，只是还在排队。把它当「未执行/已取消」
    /// 报出去，界面就会显示「6 条没执行」（卡 t_ebb1a014 的 GAP-2）。
    pub fn queued(&self) -> i64 {
        self.pending
    }

    /// 未执行（取消 / 中断后没跑）的任务数。
    ///
    /// **只在收工那一刻成立**：`finish()` 把自己的值写进 `compliance_runs.cancelled`。
    /// 运行中调用等于把「排队中」误报成「未执行」——视图侧请用 [`RunCounts::queued`]。
    pub fn cancelled(&self) -> i64 {
        self.pending
    }

    /// 恒等式自检：total = done + failed + skipped + pending
    pub fn is_balanced(&self) -> bool {
        self.total == self.done + self.failed + self.skipped + self.pending
    }
}

/// 一次执行（run）的账本 + 其任务视图。
///
/// 实现：SQLite 见 [`crate::compliance_runs_sqlite`]；后续 PG 化 = 新增平行 impl，业务层零改动。
pub trait ComplianceRunRepo {
    /// 开一次执行（status=running）。返回 `runId`。
    fn start(&self, kind: &str, params: Option<&str>) -> Result<i64>;

    /// 把**上一次没收工的合规 run** 遗留的未完成 task 认领到本轮
    /// （`runId` 非 NULL 且所属 run 已不是 running 的行；`pending` 与残留 `running` 都算）。
    ///
    /// 为什么需要：取消后剩余任务按断点续跑语义留在队列（不删、不改判），下一次判定因
    /// 「同 kind 同 target 已有 pending 就不再入队」的防重语义不会重新建行 —— 不认领它们
    /// 就会出现「任务跑了但本 run 的 total 里没有它」。残留 `running` 也必须认领：上一轮
    /// 收工时被中途叫停的 job 就停在这个状态（工作线程侧的 `requeue_stale` 要到worker 起来
    /// 才回置 pending，晚于本调用）。
    ///
    /// 迁移前的老行（`runId IS NULL`）**不在此列**：那是历史，不回填。
    fn adopt_pending(&self, run_id: i64, card_types: &[&str]) -> Result<usize>;

    /// 记一次「跳过」（skipped 唯一来源：`job_queue` 区分不出跳过与成功）。
    fn note_skipped(&self, run_id: i64) -> Result<()>;

    /// 收工：写 status/finishedAt/error，并按 `job_queue` 汇总回填 total/done/failed/cancelled
    /// （`skipped` 保留计数器的值）。返回收工后的行。
    fn finish(
        &self,
        run_id: i64,
        status: &str,
        error: Option<&str>,
        card_types: &[&str],
    ) -> Result<RunRow>;

    fn get(&self, run_id: i64) -> Result<Option<RunRow>>;

    /// 最近一次执行（不论状态）。
    fn latest(&self) -> Result<Option<RunRow>>;

    /// 正在跑的那一次（若有）。
    fn running(&self) -> Result<Option<RunRow>>;

    /// 列表（按 startedAt 倒序）：(总数, 本页)。
    fn list(&self, limit: i64, offset: i64) -> Result<(i64, Vec<RunRow>)>;

    /// run 内任务明细（按 jobId 升序）：(总数, 本页)。
    fn tasks(&self, run_id: i64, limit: i64, offset: i64) -> Result<(i64, Vec<TaskRow>)>;

    /// run 内任务计数（单一事实源 = `job_queue`）。
    fn counts(&self, run_id: i64, card_types: &[&str]) -> Result<RunCounts>;

    /// 启动自愈：所有 `status='running'` 的 run 视为「进程死在中间」→ 收工为 `interrupted`
    /// 并按队列事实回填计数（否则界面永远显示一个永远不动 100% 的进度）。
    /// 返回被自愈的行数。
    fn interrupt_stale(&self, card_types: &[&str]) -> Result<usize>;
}
