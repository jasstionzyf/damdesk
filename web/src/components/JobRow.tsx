/**
 * DM-B2（KI-37）L1 行：**所有类型共用一个行骨架**（打标/导入/导出/去重/检查/处理）。
 *
 * 一行让人一眼读出：什么时候 · 什么类型 · 用什么（方案/目录）· 多少张 · 结果怎么样 · 耗时。
 * 类型差异只在「摘要」的文字里（`UnifiedJob.summary` 已按类型算好）；行的结构与交互完全共用，
 * 新加一种类型不需要改这个文件。
 */
import type { UnifiedJob } from '../lib/jobs'
import { fmtJobDuration, fmtJobTime, jobTypeLabel } from '../lib/jobs'
import { useI18n } from '../i18n'

const STATUS_DOT: Record<UnifiedJob['status'], string> = {
  running: 'bg-accent animate-pulse',
  done: 'bg-emerald-400',
  error: 'bg-rose-400',
  cancelled: 'bg-ink-dim',
  interrupted: 'bg-amber-400',
}

const TYPE_CHIP: Record<UnifiedJob['type'], string> = {
  judge: 'border-sky-500/40 text-sky-300',
  process: 'border-violet-500/40 text-violet-300',
  import: 'border-emerald-500/40 text-emerald-300',
  export: 'border-amber-500/40 text-amber-300',
  tag: 'border-teal-500/40 text-teal-300',
  dedup: 'border-orange-500/40 text-orange-300',
  embed: 'border-slate-500/40 text-slate-300',
}

export interface JobRowProps {
  job: UnifiedJob
  active: boolean
  onOpen: (job: UnifiedJob) => void
}

export function JobRow({ job, active, onOpen }: JobRowProps) {
  const { t } = useI18n()
  const duration = fmtJobDuration(job.startedAt, job.finishedAt, job.durationMs)
  const when = job.timeNote ?? fmtJobTime(job.startedAt)
  return (
    <button
      type="button"
      onClick={() => onOpen(job)}
      aria-current={active ? 'true' : undefined}
      data-testid={`job-row-${job.key}`}
      data-job-key={job.key}
      data-job-type={job.type}
      data-status={job.status}
      data-run-id={job.runId ?? undefined}
      className={`w-full rounded border px-3 py-2 text-left transition ${
        active
          ? 'border-accent/60 bg-accent/10'
          : 'border-line bg-panel hover:border-accent/40 hover:bg-panel-2'
      }`}
    >
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
        <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${STATUS_DOT[job.status]}`} aria-hidden />
        <span
          className={`shrink-0 rounded border px-1 py-0.5 text-[10px] leading-none ${TYPE_CHIP[job.type]}`}
          data-testid={`job-row-type-${job.key}`}
        >
          {jobTypeLabel(job.type)}
        </span>
        <span className="shrink-0 tabular-nums text-ink-dim">{when}</span>
        <span className="min-w-0 max-w-72 truncate font-medium text-ink" title={job.title}>
          {job.title}
        </span>
        {job.count != null && (
          <span className="shrink-0 text-ink-dim" data-testid={`job-row-count-${job.key}`}>
            {t('{n} 张', { n: job.count })}
          </span>
        )}
        <span className="flex-1" />
        <span className="text-ink-dim" data-testid={`job-row-summary-${job.key}`}>
          {job.summary}
        </span>
        <span
          className={`shrink-0 ${
            job.status === 'error' ? 'text-rose-300' : job.status === 'running' ? 'text-accent' : 'text-ink-dim'
          }`}
          data-testid={`job-row-status-${job.key}`}
        >
          {job.statusLabel}
        </span>
        {duration && <span className="shrink-0 tabular-nums text-ink-dim">{t('耗时 {d}', { d: duration })}</span>}
      </div>

      {job.progress && (
        <div className="mt-1.5 flex items-center gap-2" data-testid={`job-row-progress-${job.key}`}>
          <div className="h-1 flex-1 overflow-hidden rounded bg-panel-2">
            <div
              className="h-full bg-accent transition-[width]"
              style={{ width: `${job.progress.total > 0 ? Math.min(100, (job.progress.current / job.progress.total) * 100) : 0}%` }}
            />
          </div>
          <span className="tabular-nums text-[10px] text-ink-dim">
            {job.progress.current} / {job.progress.total}
          </span>
        </div>
      )}

      {/* 人话身份一行（方案 / 来源 / 目标）；老探针按 `job-spec-<runId>` 读这一行 */}
      <div
        className="mt-1 truncate text-[11px] text-ink-dim"
        data-testid={job.runId != null ? `job-spec-${job.runId}` : `job-sub-${job.key}`}
      >
        {job.subtitle ? `${job.subtitle} · ` : ''}
        {job.title}
      </div>

      {/* 兼容钩子：历史探针（cmp_job/ e2e_batch）按 `compliance-job-row-<runId>` 找行并点它 */}
      {job.runId != null && (
        <span
          hidden
          data-testid={`compliance-job-row-${job.runId}`}
          data-run-id={job.runId}
          data-status={job.status}
        >
          {job.statusLabel}
        </span>
      )}
    </button>
  )
}
