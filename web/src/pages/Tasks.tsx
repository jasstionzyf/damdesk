/**
 * DM-B2（KI-37）：**统一任务列表**（L1）—— 打标 / 导入 / 导出 / 去重 / 检查 / 处理
 * 全部收进这一个列表（用户裁决 2026-10-25：所有任务抽象成 job，只是类型不同）。
 *
 * 三级下钻：
 *  L1 本页列表（运行中置顶带进度；行 = 时间 · 类型 · 方案/目录 · 多少张 · 结果摘要 · 耗时）
 *  L2 点一行 → 就地展开「这一次的结果」（容器 `JobResultShell`，内容按类型注册）
 *  L3 需要看图 → 「看这批图」开 Dialog（缩略图网格 + 结论角标 + 单图就地改判）
 *
 * 哪些是「共用」：列表骨架、状态点、进度、过滤、L2 容器、L3 Dialog、时间/耗时格式化。
 * 哪些是「分化」：只有 L2 的结果内容（检查=分布+表、处理=产物、导入=失败明细+重跑、
 * 导出=报告、打标=待确认入口）。新加一种类型 = 多注册一个 L2 内容，L1/L3 不动。
 *
 * 保留口径（别再吓人）：检查 / 处理跑过的每一次**一直都在**；只有导入批次留最近 50 次、
 * 导出报告留最近一次 —— 那是它们各自后端账本的边界，跟检查结果无关。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'

import { Button } from '../components/Button'
import { JobResultShell } from '../components/JobResultShell'
import { JobRow } from '../components/JobRow'
import { JobResultView } from '../components/JobResults'
import { ReviewQueueDialog } from '../components/ReviewQueueDialog'
import { PathLink } from '../components/PathLink'
import { TaskDetail } from '../components/TaskCenter'
import { api, type ExportStatus, type ImportBatchRecord } from '../lib/api'
import { useOverlay } from '../lib/modal'
import { useJobsFeed } from '../lib/jobsFeed'
import { useI18n } from '../i18n'
import {
  JOB_FILTERS,
  countJobs,
  filterJobs,
  fmtJobTime,
  type JobFilterKey,
  type UnifiedJob,
} from '../lib/jobs'

function fmtNum(n: number): string {
  return n.toLocaleString('zh-CN')
}

const BATCH_STATUS: Record<string, string> = {
  running: '进行中',
  done: '已完成',
  failed: '失败',
  cancelled: '已取消',
}

const STORE_LABEL: Record<string, string> = {
  copyin: '复制入库',
  reference: '原地索引',
}

/** L2 · 导入（没有在跑的任务行时回看后端批次记录：源目录可点开、失败原因照读） */
function ImportBatchDetail({ batchId }: { batchId: number }) {
  const { t } = useI18n()
  const [row, setRow] = useState<ImportBatchRecord | null>(null)
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    api
      .importBatches()
      .then((rows) => {
        if (!alive) return
        setRow(rows.find((r) => r.batchId === batchId) ?? null)
      })
      .catch((e) => {
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      })
    return () => {
      alive = false
    }
  }, [batchId])

  if (err) {
    return (
      <p className="break-all text-xs text-rose-300" data-testid="job-import-detail-error">
        {t('读取导入批次失败：{err}', { err })}
      </p>
    )
  }
  if (!row) {
    return (
      <p className="text-xs text-ink-dim" data-testid="job-import-detail-empty">
        {t('这条批次记录不在了（后端只留最近 50 次）。')}
      </p>
    )
  }
  return (
    <div className="space-y-2 text-xs" data-testid="job-import-detail">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-ink-dim">
        <span className="text-ink">
          {t('{a}/{b} 个文件', { a: fmtNum(row.doneFiles), b: fmtNum(row.totalFiles) })}
        </span>
        <span>{BATCH_STATUS[row.status] ? t(BATCH_STATUS[row.status]) : row.status}</span>
        <span>{STORE_LABEL[row.storeMode] ? t(STORE_LABEL[row.storeMode]) : row.storeMode}</span>
        <span>{fmtJobTime(row.startedAt * 1000)}</span>
      </div>
      {row.error && <p className="break-all text-rose-300">{row.error}</p>}
      <div>
        <p className="text-ink-dim">{t('源目录')}</p>
        <PathLink path={row.rootPath} mode="open" className="mt-0.5" />
      </div>
    </div>
  )
}

/** L2 · 导出（当前运行 + 最近一次报告；后端只有这两份，如实说） */
function ExportDetail() {
  const { t } = useI18n()
  const [st, setSt] = useState<ExportStatus | null>(null)
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    let timer: number | undefined
    const tick = async () => {
      try {
        const s = await api.exportStatus()
        if (!alive) return
        setSt(s)
        setErr(null)
        if (s.running) timer = window.setTimeout(tick, 1000)
      } catch (e) {
        if (!alive) return
        setErr(e instanceof Error ? e.message : String(e))
      }
    }
    void tick()
    return () => {
      alive = false
      if (timer) window.clearTimeout(timer)
    }
  }, [])

  const r = st?.report ?? null
  return (
    <div className="space-y-2 text-xs" data-testid="job-export-detail">
      <p className="leading-5" data-testid="export-last-summary">
        {r ? (
          <>
            {t('最近一次：导出')} <b>{fmtNum(r.exported)}</b>/{fmtNum(r.total)}{t(' 张')}
            {r.skipped.length > 0 && <>{t(' · 跳过 {n} 张', { n: fmtNum(r.skipped.length) })}</>}
            {r.errors.length > 0 && <>{t(' · 失败 {n} 条', { n: fmtNum(r.errors.length) })}</>}
            {r.cancelled && (
              <span className="ml-2 rounded bg-amber-600/20 px-1.5 py-0.5 text-amber-300">{t('已取消')}</span>
            )}
          </>
        ) : st?.running ? (
          t('本轮导出进行中…')
        ) : (
          <span className="text-ink-dim">{t('最近还没有导出记录')}</span>
        )}
      </p>

      {err && (
        <p className="break-all text-rose-300" data-testid="job-export-detail-error">
          {t('读取导出状态失败：{err}', { err })}
        </p>
      )}

      {st?.running && st.progress && (
        <div className="rounded border border-accent/40 bg-accent-soft/40 p-2.5" data-testid="export-running">
          <p>
            {t('正在导出 · {phase} · {a}/{b}', {
              phase: st.progress.phase,
              a: fmtNum(st.progress.done),
              b: fmtNum(st.progress.total),
            })}
          </p>
          <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-panel">
            <div
              className="h-full rounded-full bg-accent transition-all"
              style={{
                width: `${st.progress.total > 0 ? Math.min(100, Math.round((st.progress.done / st.progress.total) * 100)) : 0}%`,
              }}
            />
          </div>
          {st.progress.current && (
            <p className="mt-1 truncate text-[11px] text-ink-dim" title={st.progress.current}>
              {st.progress.current}
            </p>
          )}
          <Button
            type="button"
            variant="secondary"
            onClick={() =>
              void api.exportCancel().then(() => setSt((s) => (s ? { ...s, running: false } : s)))
            }
          >
            {t('取消导出')}
          </Button>
        </div>
      )}

      {r ? (
        <div className="space-y-1.5" data-testid="export-report">
          <div className="text-ink-dim">
            <p>{t('导出目标')}{r.manifestWritten ? <span className="ml-2">{t('· 已写 manifest.csv')}</span> : null}</p>
            <PathLink path={r.dest} mode="open" className="mt-0.5" />
          </div>
          {r.skipped.length > 0 && (
            <details className="rounded border border-line px-2 py-1.5" data-testid="export-skipped">
              <summary className="cursor-pointer text-ink-dim">{t('跳过的 {n} 张（逐条原因）', { n: r.skipped.length })}</summary>
              <ul className="mt-1 space-y-0.5">
                {r.skipped.map((s) => (
                  <li key={s.assetId} className="truncate" title={`${s.fileName}：${s.reason}`}>
                    <span className="font-mono text-ink-dim">{s.fileName}</span> —— {s.reason}
                  </li>
                ))}
              </ul>
            </details>
          )}
          {r.errors.length > 0 && (
            <details className="rounded border border-line px-2 py-1.5" data-testid="export-errors">
              <summary className="cursor-pointer text-ink-dim">{t('失败原因 {n} 条', { n: r.errors.length })}</summary>
              <ul className="mt-1 space-y-0.5">
                {r.errors.slice(0, 50).map((e, i) => (
                  <li key={`${i}-${e}`} className="break-all">
                    {e}
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
      ) : (
        !st?.running && (
          <p className="text-ink-dim" data-testid="export-report-empty">
            {t('导出完成后，这一轮的进程与报告会留在这里可回看。')}
          </p>
        )
      )}
    </div>
  )
}

/** L2 内容按类型注册：新加一种任务类型只加一条，L1/L3 与流程骨架不动 */
function JobDetail({ job, onClose, onChanged }: { job: UnifiedJob; onClose: () => void; onChanged: () => void }) {
  const { t } = useI18n()
  if (job.type === 'verdict' || job.type === 'artifact') {
    return <JobResultView job={job} onClose={onClose} onChanged={onChanged} />
  }
  if (job.type === 'export') {
    return (
      <JobResultShell
        title={t('这一次的导出')}
        subtitle={job.summary}
        onClose={onClose}
        testid="job-export-result"
        actions={
          <Link to="/?dialog=export" className="text-[11px] text-accent hover:underline">
            {t('去导出 →')}
          </Link>
        }
      >
        <ExportDetail />
      </JobResultShell>
    )
  }
  if (job.taskId != null) {
    return (
      <JobResultShell
        title={t('这一次的结果')}
        subtitle={job.subtitle ?? job.title}
        onClose={onClose}
        testid="job-task-result"
        actions={
          job.type === 'tag' ? (
            <Link to="/tag-review" className="text-[11px] text-accent hover:underline" data-testid="job-tag-review-link">
              {t('去待确认 →')}
            </Link>
          ) : null
        }
      >
        <TaskDetail taskId={job.taskId} />
      </JobResultShell>
    )
  }
  if (job.type === 'import' && job.batchId != null) {
    return (
      <JobResultShell title={t('这一次的导入')} subtitle={job.subtitle ?? job.title} onClose={onClose} testid="job-import-result">
        <ImportBatchDetail batchId={job.batchId} />
      </JobResultShell>
    )
  }
  return (
    <JobResultShell title={t('这一次的结果')} subtitle={job.summary} onClose={onClose} testid="job-empty-result">
      <p className="text-xs text-ink-dim">{t('这条记录没有可展开的明细（后端只留了计数）。')}</p>
    </JobResultShell>
  )
}

export function Tasks() {
  const { t } = useI18n()
  const [params, setParams] = useSearchParams()
  const { jobs, loading, error, refresh } = useJobsFeed()
  const [filter, setFilter] = useState<JobFilterKey>('all')
  /** DECouple-FE：复核队列 = 低频 Dialog（原 /review 独立页下线后的唯一入口） */
  const reviewOverlay = useOverlay('dialog', 'review-queue')
  const [reviewCount, setReviewCount] = useState(0)
  const refreshReviewCount = useCallback(() => {
    // 计数取不到（老后端 / 插件未装 / 单测桩）一律按 0 —— 这一行不该让任务中心打不开
    void Promise.resolve()
      .then(() => api.complianceReview({ lowConfidence: true, limit: 1 }))
      .then((r) => setReviewCount(r?.total ?? 0))
      .catch(() => setReviewCount(0))
  }, [])
  useEffect(() => {
    refreshReviewCount()
  }, [refreshReviewCount])

  const counts = useMemo(() => countJobs(jobs), [jobs])
  const shown = useMemo(() => filterJobs(jobs, filter), [jobs, filter])
  const running = useMemo(() => jobs.filter((j) => j.status === 'running').length, [jobs])

  const selected = useMemo(() => {
    const runId = Number(params.get('job')) || null
    const taskId = Number(params.get('task')) || null
    const batchId = Number(params.get('batch')) || null
    const isExport = params.get('export') === '1'
    if (runId != null) return jobs.find((j) => j.runId === runId) ?? null
    if (taskId != null) return jobs.find((j) => j.taskId === taskId) ?? null
    if (batchId != null) return jobs.find((j) => j.batchId === batchId && j.taskId == null) ?? null
    if (isExport) return jobs.find((j) => j.type === 'export') ?? null
    return null
  }, [jobs, params])

  /** 深链定位提示（`?job=7` / `?task=42` / `?batch=41`）：刷新、分享、从 toast 点进来都能对上号 */
  const focusId = params.get('job') ?? params.get('task') ?? params.get('batch')

  const open = (job: UnifiedJob) => {
    if (job.runId != null) setParams({ job: String(job.runId) })
    else if (job.taskId != null) setParams({ task: String(job.taskId) })
    else if (job.type === 'import' && job.batchId != null) setParams({ batch: String(job.batchId) })
    else setParams({ export: '1' })
  }
  const close = () => setParams({})

  return (
    <div className="mx-auto flex w-full max-w-4xl flex-col gap-4 overflow-y-auto p-6" data-testid="page-tasks">
      <header>
        <h1 className="text-lg font-semibold">
          {t('处理记录')}
          {running > 0 && (
            <span className="ml-2 rounded-full bg-accent-soft px-2 py-0.5 text-xs font-normal text-ink">
              {t('{n} 进行中', { n: running })}
            </span>
          )}
        </h1>
        <p className="mt-1 text-xs leading-5 text-ink-dim">
          {t('打标 / 导入 / 导出 / 去重 / 检查 / 处理都在这一张列表里。点一行看那一次的结果，需要看图时再点「看这批图」。')}
        </p>
        <p className="mt-1 text-xs leading-5 text-ink-dim" data-testid="tasks-retention-note">
          {t('检查与处理的每一次结果一直都在（按每次分开、不覆盖）；导入批次留最近 50 次记录、导出报告留最近一次 ——只是这两类后端只存这么多，跟检查结果无关。')}
        </p>
        {focusId && (
          <p className="mt-1 text-xs text-accent" data-testid="task-focus-hint">
            {t('已定位到记录 #{id}', { id: focusId })}
          </p>
        )}
      </header>

      <section className="rounded-lg border border-line bg-panel p-4" data-testid="job-list-section">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-sm font-semibold">{t('全部任务（{n}）', { n: jobs.length })}</h2>
          <div className="flex items-center gap-2">
            <span className="text-[11px] text-ink-dim" data-testid="job-list-total">
              {loading ? t('读取中…') : t('共 {n} 条', { n: jobs.length })}
            </span>
            {/* DECouple-FE：判定/处理的一切都在任务中心闭环；「待人工看」是跨 job 的低频动作 → Dialog */}
            <Button
              type="button"
              variant="secondary"
              onClick={() => reviewOverlay.openOverlay()}
              data-testid="tasks-review-queue"
            >
              {t('待人工看（{n}）', { n: reviewCount })}
            </Button>
            <Button type="button" variant="secondary" disabled={loading} onClick={refresh}>
              {loading ? t('刷新中…') : t('刷新')}
            </Button>
          </div>
        </div>

        {error && (
          <p className="mt-2 break-all text-xs text-rose-300" data-testid="job-list-error">
            {t('读取处理记录失败：{err}', { err: error })}
          </p>
        )}

        {jobs.length > 0 && (
          <div
            className="mt-3 flex flex-wrap items-center gap-1"
            data-testid="job-list-filter"
            role="group"
            aria-label={t('按类型筛选处理记录')}
          >
            {JOB_FILTERS.map((f) => (
              <Button
                key={f.key}
                type="button"
                variant="secondary"
                size="none"
                active={filter === f.key}
                onClick={() => setFilter(f.key)}
                className="h-6 px-2 text-[11px]"
                data-testid={`job-list-filter-${f.key}`}
              >
                {t(f.label)} {counts[f.key]}
              </Button>
            ))}
          </div>
        )}

        {!loading && jobs.length === 0 && (
          <p className="mt-3 text-xs leading-5 text-ink-dim" data-testid="job-list-empty">
            {t('还没有跑过任何任务：在图库选图后点「送去处理 →」发起检查或处理，或从「导入」开始一批。跑过的每一次都会留在这里。')}
          </p>
        )}

        {jobs.length > 0 && shown.length === 0 && (
          <p className="mt-3 text-xs text-ink-dim" data-testid="job-list-filter-empty">
            {t('这个类型下还没有记录：换个筛选，或先去跑一次。')}
          </p>
        )}

        {shown.length > 0 && (
          <div className="mt-3 flex flex-col gap-1.5" data-testid="job-list">
            {shown.map((job) => (
              <div key={job.key}>
                <JobRow job={job} active={selected?.key === job.key} onOpen={open} />
                {selected?.key === job.key && (
                  <div className="mt-2" data-testid="job-detail-panel">
                    <JobDetail job={job} onClose={close} onChanged={refresh} />
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
      </section>

      <ReviewQueueDialog
        open={reviewOverlay.open}
        onClose={reviewOverlay.closeOverlay}
        onChanged={() => {
          refreshReviewCount()
          refresh()
        }}
      />
    </div>
  )
}
