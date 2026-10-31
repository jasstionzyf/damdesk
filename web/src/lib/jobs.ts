/**
 * DM-B2（KI-37）：**统一任务模型** —— 打标 / 导入 / 导出 / 去重 / 检查 / 处理都是「一次执行」，
 * 只是类型不同。处理记录页（`/tasks`）的 L1 列表只吃这一份行，不再让用户面对四段各自为政的列表。
 *
 * 设计口径（用户裁决 2026-10-25，见 damdesk-product `job-task-semantics.md`「统一 Job 抽象」）：
 *  - **共用**：L1 行骨架 / 状态点 / 进度 / 过滤框架 / L2 容器 / L3 图列表 —— 都是本文件这份行数据；
 *  - **分化**：只有 L2 的「结果内容区」按 `type` 注册（检查=判定分布+表、处理=产物、
 *    导入=失败明细+重跑、导出=报告、打标=待确认入口）。
 *
 * 存储不合并（job_queue run 行 / compliance run 行 / 导入批次各留各的），
 * 这里做**视图层合并**：每类来源一个 `*ToRow()`，之后 `mergeJobs()` 去重 + 排序。
 */
import type { ComplianceJobView, ExportStatus, ImportBatchRecord } from './api'
import type { Task } from './tasks'
import { verdictLabels } from './pluginResult'
import { getGlobalLocale, tGlobal, type LocaleCode } from '../i18n'

export type JobType = 'judge' | 'process' | 'import' | 'export' | 'tag' | 'dedup' | 'embed'

/** 统一状态词：进度语义对齐（合规账本叫 failed/interrupted，本地任务叫 error） */
export type JobStatus = 'running' | 'done' | 'error' | 'cancelled' | 'interrupted'

export interface JobVerdictCounts {
  pass: number
  fail: number
  undetermined: number
}

export interface UnifiedJob {
  /** 稳定身份（列表 key / 深链定位）：`compliance:7` / `task:3` / `import:12` / `export:last` */
  key: string
  type: JobType
  status: JobStatus
  statusLabel: string
  /** epoch 毫秒 */
  startedAt: number
  finishedAt: number | null
  /** 人话身份：方案名 / 任务标题 / 源目录名 */
  title: string
  /** 副标题：能力说明 / 存放方式 / 导出目标 */
  subtitle: string | null
  /** 这次涉及多少张 */
  count: number | null
  /** 结果摘要（按类型分化：判定分布 / 成功失败跳过 / 文件数 / 张数） */
  summary: string
  /** 图级判定分布（只有检查类有；处理类没有「合格/不合格」） */
  verdicts: JobVerdictCounts | null
  /** 运行中才有 */
  progress: { current: number; total: number } | null
  /** 来源账本里的主键（L2 取结果用） */
  runId: number | null
  taskId: number | null
  batchId: number | null
  /** 类型化深链（可分享/刷新可恢复） */
  href: string
  /** 后端给的耗时（毫秒；收工的行才有） */
  durationMs: number | null
  /** 时间一列的特殊说明（导出只有「最近一次」，没有时间戳） */
  timeNote: string | null
}

/** 任务类型/状态的中文原文键（i18n-D：取词走 jobTypeLabel()/statusLabelOf()，勿直读常量） */
export const JOB_TYPE_LABEL: Record<JobType, string> = {
  judge: '检查',
  process: '处理',
  import: '导入',
  export: '导出',
  tag: '打标',
  dedup: '去重',
  embed: '嵌入',
}

export const JOB_STATUS_LABEL: Record<JobStatus, string> = {
  running: '进行中',
  done: '已完成',
  error: '失败',
  cancelled: '已取消',
  interrupted: '已中断',
}

/** i18n-D：任务类型标签按当前 locale 取词（渲染时调用，locale 切换即生效） */
export function jobTypeLabel(type: JobType): string {
  return tGlobal(JOB_TYPE_LABEL[type])
}

/** i18n-D：任务状态标签按当前 locale 取词 */
export function statusLabelOf(status: JobStatus): string {
  return tGlobal(JOB_STATUS_LABEL[status])
}

/** 这一次是检查还是处理 —— 与「建方案」页同一判定口径：规格快照 `actions` 非空 = 处理。 */
export function complianceJobType(job: ComplianceJobView): 'judge' | 'process' {
  const actions = (job.params as { spec?: { actions?: unknown[] } } | null)?.spec?.actions
  return Array.isArray(actions) && actions.length > 0 ? 'process' : 'judge'
}

/** 这一次用了哪条方案（N1 起写在 params.spec 快照里；老行没有 → null） */
export function specNameOf(job: ComplianceJobView): string | null {
  const spec = (job.params as { spec?: { name?: string } } | null)?.spec
  return spec?.name ?? null
}

export function complianceStatusOf(status: string): JobStatus {
  if (status === 'failed') return 'error'
  if (status === 'interrupted') return 'interrupted'
  if (status === 'cancelled') return 'cancelled'
  if (status === 'running') return 'running'
  return 'done'
}

/** 三态分布的人话（与结果表同一套标签：通过 / 未通过 / 待人工看，不另造词） */
function verdictSummary(v: JobVerdictCounts): string {
  const L = verdictLabels()
  return `${L.pass} ${v.pass} · ${L.fail} ${v.fail} · ${L.undetermined} ${v.undetermined}`
}

/** 本次数值的人话（成功/失败/跳过），检查与处理两型共用 */
function countsSummary(done: number, failed: number, skipped: number): string {
  return tGlobal('成功 {done} · 失败 {failed} · 跳过 {skipped}', { done, failed, skipped })
}

/** 合规账本的一行 → 统一行（检查 / 处理两型） */
export function complianceJobToRow(job: ComplianceJobView): UnifiedJob {
  const type = complianceJobType(job)
  const spec = specNameOf(job)
  const status = complianceStatusOf(job.status)
  const counts = job.verdictCounts ?? null
  const summary =
    type === 'judge'
      ? counts
        ? verdictSummary(counts)
        : countsSummary(job.done, job.failed, job.skipped)
      : countsSummary(job.done, job.failed, job.skipped)
  const count = job.totalAssets ?? (type === 'judge' ? null : job.total)
  return {
    key: `compliance:${job.runId}`,
    type,
    status,
    statusLabel: statusLabelOf(status),
    startedAt: job.startedAt * 1000,
    finishedAt: job.finishedAt ? job.finishedAt * 1000 : null,
    title: spec ?? tGlobal('方案未记录'),
    subtitle: jobTypeLabel(type),
    count,
    summary,
    verdicts: type === 'judge' ? counts : null,
    progress: status === 'running' ? { current: job.done + job.failed + job.skipped, total: job.total } : null,
    runId: job.runId,
    taskId: null,
    batchId: null,
    href: `/tasks?job=${job.runId}`,
    durationMs: job.durationMs ?? (job.finishedAt ? (job.finishedAt - job.startedAt) * 1000 : null),
    timeNote: null,
  }
}

/** 本地后台任务（导入 / 嵌入 / 打标 / 去重 / 导出）→ 统一行 */
export function taskToRow(t: Task): UnifiedJob {
  const status: JobStatus =
    t.status === 'error' ? 'error' : t.status === 'cancelled' ? 'cancelled' : t.status === 'running' ? 'running' : 'done'
  const failed = t.failed ?? 0
  const summary =
    status === 'running'
      ? failed > 0
        ? tGlobal('{current} / {total} · 已失败 {failed}', { current: t.current, total: t.total, failed })
        : tGlobal('{current} / {total}', { current: t.current, total: t.total })
      : status === 'done' && failed > 0
        ? countsSummary(Math.max(0, t.total - failed), failed, 0)
        : status === 'done'
          ? countsSummary(t.total, 0, 0)
          : status === 'cancelled'
            ? tGlobal('已处理 {current} / {total}', { current: t.current, total: t.total })
            : tGlobal('失败于第 {n} 项', { n: t.current })
  return {
    key: t.kind === 'import' && t.batchId != null ? `import:${t.batchId}` : `task:${t.id}`,
    type: t.kind,
    status,
    statusLabel: statusLabelOf(status),
    startedAt: t.startedAt,
    finishedAt: null,
    title: t.title,
    subtitle: t.detail || null,
    count: t.total || null,
    summary,
    verdicts: null,
    progress: status === 'running' ? { current: t.current, total: t.total } : null,
    runId: null,
    taskId: t.id,
    batchId: t.batchId ?? null,
    href: `/tasks?task=${t.id}`,
    durationMs: null,
    timeNote: null,
  }
}

/** i18n-D：入库方式标签每次调用取词（copyin / reference 之外原样显示后端值） */
export function batchStoreLabel(storeMode: string): string {
  if (storeMode === 'copyin') return tGlobal('复制入库')
  if (storeMode === 'reference') return tGlobal('原地索引')
  return storeMode
}

/** 导入批次记录（后端最近 50 条）→ 统一行（在跑的导入任务优先，见 mergeJobs 去重） */
export function importBatchToRow(b: ImportBatchRecord): UnifiedJob {
  const status: JobStatus =
    b.status === 'running'
      ? 'running'
      : b.status === 'failed'
        ? 'error'
        : b.status === 'cancelled'
          ? 'cancelled'
          : 'done'
  const root = b.rootPath.replace(/[\\/]+$/, '')
  const name = root.split(/[\\/]/).pop() || b.rootPath
  return {
    key: `import:${b.batchId}`,
    type: 'import',
    status,
    statusLabel: statusLabelOf(status),
    startedAt: b.startedAt * 1000,
    finishedAt: b.endedAt ? b.endedAt * 1000 : null,
    title: name,
    subtitle: batchStoreLabel(b.storeMode),
    count: b.totalFiles,
    summary: tGlobal('{done} / {total} 个文件', { done: b.doneFiles, total: b.totalFiles }),
    verdicts: null,
    progress: status === 'running' ? { current: b.doneFiles, total: b.totalFiles } : null,
    runId: null,
    taskId: null,
    batchId: b.batchId,
    href: `/tasks?batch=${b.batchId}`,
    durationMs: b.endedAt ? (b.endedAt - b.startedAt) * 1000 : null,
    timeNote: null,
  }
}

/** 导出（后端只留当前运行 + 最近一次报告）→ 统一行；两者都没有 → null（不编造历史） */
export function exportToRow(st: ExportStatus): UnifiedJob | null {
  const r = st.report
  if (!r && !st.running) return null
  const running = !!st.running
  const done = r?.exported ?? st.progress?.done ?? 0
  const total = r?.total ?? st.progress?.total ?? 0
  const summary = r
    ? tGlobal('成功 {done} 张 · 跳过 {skipped} 张{errors}', {
        done: r.exported,
        skipped: r.skipped.length,
        errors: r.errors.length ? tGlobal(' · 失败 {n} 条', { n: r.errors.length }) : '',
      })
    : tGlobal('正在导出 {done} / {total}', { done, total })
  return {
    key: 'export:last',
    type: 'export',
    status: running ? 'running' : r?.cancelled ? 'cancelled' : 'done',
    statusLabel: running ? statusLabelOf('running') : r?.cancelled ? statusLabelOf('cancelled') : statusLabelOf('done'),
    // 后端没留时间戳：按「最近一次」放在运行中之下、其余之上（`timeNote` 直接说清）
    startedAt: Date.now(),
    finishedAt: null,
    title: r?.dest ?? tGlobal('导出'),
    subtitle: tGlobal('导出到目录'),
    count: total,
    summary,
    verdicts: null,
    progress: running && st.progress ? { current: st.progress.done, total: st.progress.total } : null,
    runId: null,
    taskId: null,
    batchId: null,
    href: '/tasks?export=1',
    durationMs: null,
    timeNote: tGlobal('最近一次'),
  }
}

/**
 * 合并 + 排序：同一件事只留一行（在跑的导入任务比批次记录更实 → 任务赢），
 * 运行中的置顶，其余按开始时间倒序。
 */
export function mergeJobs(rows: UnifiedJob[]): UnifiedJob[] {
  const byKey = new Map<string, UnifiedJob>()
  for (const row of rows) {
    const prev = byKey.get(row.key)
    if (!prev || (prev.status !== 'running' && row.status === 'running')) byKey.set(row.key, row)
  }
  return [...byKey.values()].sort((a, b) => {
    const ar = a.status === 'running' ? 0 : 1
    const br = b.status === 'running' ? 0 : 1
    return ar - br || b.startedAt - a.startedAt
  })
}

export type JobFilterKey = 'all' | 'running' | 'judge' | 'process' | 'io' | 'tag' | 'other'

/** 筛选键 → 人话标签（i18n-D：函数取词；JOB_FILTERS 常量的中文值仅作键表） */
export function jobFilterLabel(key: JobFilterKey): string {
  switch (key) {
    case 'all':
      return tGlobal('全部')
    case 'running':
      return tGlobal('进行中')
    case 'judge':
      return tGlobal('检查')
    case 'process':
      return tGlobal('处理')
    case 'io':
      return tGlobal('导入导出')
    case 'tag':
      return tGlobal('打标')
    case 'other':
      return tGlobal('其他')
  }
}

export const JOB_FILTERS: { key: JobFilterKey; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'running', label: '进行中' },
  { key: 'judge', label: '检查' },
  { key: 'process', label: '处理' },
  { key: 'io', label: '导入导出' },
  { key: 'tag', label: '打标' },
  { key: 'other', label: '其他' },
]

function inFilter(row: UnifiedJob, key: JobFilterKey): boolean {
  switch (key) {
    case 'all':
      return true
    case 'running':
      return row.status === 'running'
    case 'io':
      return row.type === 'import' || row.type === 'export'
    case 'tag':
      return row.type === 'tag'
    case 'other':
      return row.type === 'dedup' || row.type === 'embed'
    default:
      return row.type === key
  }
}

export function filterJobs(rows: UnifiedJob[], key: JobFilterKey): UnifiedJob[] {
  return key === 'all' ? rows : rows.filter((r) => inFilter(r, key))
}

export function countJobs(rows: UnifiedJob[]): Record<JobFilterKey, number> {
  const out = { all: rows.length, running: 0, judge: 0, process: 0, io: 0, tag: 0, other: 0 }
  for (const r of rows) for (const f of JOB_FILTERS) if (f.key !== 'all' && inFilter(r, f.key)) out[f.key] += 1
  return out
}

/** 英文 12 小时制时刻 + 月份缩写（formatDateTime en 档用，手写不引 dayjs） */
const EN_MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/**
 * i18n-D：时间戳 → locale 感知展示（唯一实现，别处禁止手写日期格式）。
 *  - zh-Hans：`MM-DD HH:mm`，同年省年份、跨年补 `YYYY-`（原 fmtJobTime 口径）；
 *  - en-US：`MMM D, YYYY, h:mm A`（12 小时制 + am/pm，如 `Oct 25, 2026, 2:03 PM`）。
 */
export function formatDateTime(ts: number, locale: LocaleCode = getGlobalLocale()): string {
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  if (locale === 'en-US') {
    const h24 = d.getHours()
    const h12 = h24 % 12 === 0 ? 12 : h24 % 12
    const ap = h24 < 12 ? 'AM' : 'PM'
    return `${EN_MONTHS[d.getMonth()]} ${d.getDate()}, ${d.getFullYear()}, ${h12}:${pad(d.getMinutes())} ${ap}`
  }
  const md = `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
  const now = new Date()
  return d.getFullYear() === now.getFullYear() ? md : `${d.getFullYear()}-${md}`
}

/** 时间一列（同一天不重复年份，跨年才带年；zh/en 各按 formatDateTime 口径） */
export function fmtJobTime(ms: number): string {
  return formatDateTime(ms)
}

export function fmtJobDuration(startedAt: number, finishedAt: number | null, durationMs?: number | null): string {
  const ms = durationMs ?? (finishedAt != null ? finishedAt - startedAt : null)
  if (ms == null || ms < 0) return ''
  if (ms < 1000) return `${ms}ms`
  const s = ms / 1000
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`
  const m = Math.floor(s / 60)
  const rest = Math.round(s - m * 60)
  if (getGlobalLocale() === 'en-US') return `${m}m ${rest}s`
  return `${m}分${rest}秒`
}
