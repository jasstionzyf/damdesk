/**
 * CMP-JOB-02：**一次批量审核 = 一个独立 job** 的前端口径（纯函数，node 单测覆盖）。
 *
 * 用户裁决（2026-10-17）：一次批量审核跑完要给这次出结果统计（成功 / 失败 / 跳过数），
 * **各次之间必须明显分开**；否则界面上「历史累计失败 30」与「本次失败 0」并排显示，
 * 用户会以为系统一直在坏。后端已按 run 落账（`compliance_runs`），这里只负责：
 *
 * - 计数口径单向读取（`total = done + failed + skipped + cancelled`，前端不改写、不重算成另一套）；
 * - 状态词 → 人话 / 语气（配色唯一入口，页面与抽屉都从这里取）；
 * - 时间与耗时的显示格式（时间轴要能对上「17:03 这次」）；
 * - task 的 lastError → 「跳过原因 / 失败原因」的可读解释（后端把跳过原因也写在 lastError）。
 *
 * 铁律：**失败只算本次执行内的**。历史累计（迁移前的老行 runId = NULL）永远不进这里的计数。
 */
import type { ComplianceJobView, ComplianceJobTaskView } from './api'
import { tGlobal } from '../i18n'

/** 正在跑的那一次（后端 run_status::RUNNING） */
export const RUNNING = 'running'

export type JobTone = 'running' | 'ok' | 'warn' | 'bad' | 'idle'

/** 状态 → 语气（配色唯一口径；`unknown` 也要能显示，不吞掉未来新增状态） */
export function jobTone(status: string): JobTone {
  switch (status) {
    case RUNNING:
      return 'running'
    case 'done':
      return 'ok'
    case 'cancelled':
      return 'warn'
    case 'interrupted':
    case 'failed':
      return 'bad'
    default:
      return 'idle'
  }
}

/** 状态徽章样式（Tailwind 类；tone 来自 jobTone，禁止另写一套） */
export function jobStatusClass(status: string): string {
  switch (jobTone(status)) {
    case 'running':
      return 'border-accent/60 bg-accent-soft text-ink'
    case 'ok':
      return 'border-line bg-panel-2 text-ink'
    case 'warn':
      return 'border-amber-600/60 bg-amber-600/10 text-amber-300'
    case 'bad':
      return 'border-rose-600/60 bg-rose-600/10 text-rose-300'
    default:
      return 'border-line bg-panel-2 text-ink-dim'
  }
}

/** 状态词 → 人话。后端 `statusLabel` 优先（服务端是唯一口径），缺失时这里兜底。 */
export function jobStatusLabel(status: string, served?: string | null): string {
  if (served && served.trim() !== '' && served !== '未知状态') return served
  switch (status) {
    case RUNNING:
      return tGlobal('进行中')
    case 'done':
      return tGlobal('已完成')
    case 'cancelled':
      return tGlobal('已取消')
    case 'interrupted':
      return tGlobal('已中断（进程重启）')
    case 'failed':
      return tGlobal('执行失败')
    default:
      return status || tGlobal('未知状态')
  }
}

export interface JobCounts {
  total: number
  /** 成功（不含 skipped） */
  done: number
  failed: number
  /** 跳过（文件不是图片 / 已失联 / 已删除）—— 不是失败 */
  skipped: number
  /** 取消后未执行的任务数（运行中恒 0，见 queued） */
  cancelled: number
  /** 排队中 + 执行中（运行中才有；收工后恒 0） */
  queued: number
}

function n(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : 0
}

/** 读取一次执行的计数（只读后端字段，缺失按 0；不做任何「口径修正」） */
export function jobCounts(job: ComplianceJobView): JobCounts {
  return {
    total: n(job.total),
    done: n(job.done),
    failed: n(job.failed),
    skipped: n(job.skipped),
    cancelled: n(job.cancelled),
    queued: n(job.queued),
  }
}

/**
 * 「未执行 N」文案（列表行 / 明细共用唯一入口）。
 *
 * 两道闸门：① 只有**收工**的执行才谈得上「未执行」（运行中的任务都在队列里，
 * 后端已把 running 的 cancelled 置 0，这里再挡一次，老后端也不会误显示）；
 * ② 计数为 0 不显示。返回 null = 不该出现这段文案。
 */
export function cancelledNote(job: ComplianceJobView): string | null {
  if (job.status === RUNNING) return null
  const c = jobCounts(job).cancelled
  return c > 0 ? tGlobal('未执行 {n}', { n: c }) : null
}

/** 「排队 N」文案（运行中进度卡用；收工后/为 0 → null） */
export function queuedNote(job: ComplianceJobView): string | null {
  if (job.status !== RUNNING) return null
  const q = jobCounts(job).queued
  return q > 0 ? tGlobal('排队 {n}', { n: q }) : null
}

/** 已处理条数（进度条分子）：成功 + 失败 + 跳过；取消后未执行的不算处理过 */
export function processed(job: ComplianceJobView): number {
  const c = jobCounts(job)
  return c.done + c.failed + c.skipped
}

/** 进度百分比（0–100；total 为 0 时返回 0，不显示 NaN 宽度的进度条） */
export function progressPct(job: ComplianceJobView): number {
  const t = jobCounts(job).total
  if (t <= 0) return 0
  return Math.min(100, Math.round((processed(job) / t) * 100))
}

/** 一行结果统计（列表行主文案）：`成功 24 / 失败 0 / 跳过 0 / 共 24` */
export function countsText(job: ComplianceJobView): string {
  const c = jobCounts(job)
  const note = cancelledNote(job)
  const tail = note ? tGlobal(' / {note}', { note }) : ''
  return tGlobal('成功 {done} / 失败 {failed} / 跳过 {skipped} / 共 {total}{tail}', {
    done: c.done,
    failed: c.failed,
    skipped: c.skipped,
    total: c.total,
    tail,
  })
}

/** 紧凑版（进度卡里用）：`成 12 · 败 1 · 跳 3` */
export function countsShort(job: ComplianceJobView): string {
  const c = jobCounts(job)
  return tGlobal('成 {done} · 败 {failed} · 跳 {skipped}', { done: c.done, failed: c.failed, skipped: c.skipped })
}

/** 本次执行里有没有「图级失败」——决定徽章是「已完成」还是「已完成（有失败）」 */
export function hasFailures(job: ComplianceJobView): boolean {
  return jobCounts(job).failed > 0
}

/** 结果徽章的完整人话（先看状态，再看失败数；「完成」不等于「全部成功」） */
export function jobHeadline(job: ComplianceJobView): string {
  const label = jobStatusLabel(job.status, job.statusLabel)
  if (job.status === 'done' && hasFailures(job)) return tGlobal('{label}（有失败）', { label })
  return label
}

export function isRunning(job: ComplianceJobView | null | undefined): boolean {
  return !!job && job.status === RUNNING
}

function pad(v: number): string {
  return v < 10 ? `0${v}` : String(v)
}

/** 时间戳（秒）→ 本地 `MM-DD HH:MM`（时间轴口径；秒级不上列表，免得噪声） */
export function formatClock(ts: number | null | undefined): string {
  if (ts == null || !Number.isFinite(ts) || ts <= 0) return '—'
  const d = new Date(ts * 1000)
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** 时间戳（秒）→ 本地 `MM-DD HH:MM:SS`（明细里用，毫秒级对照日志） */
export function formatClockSec(ts: number | null | undefined): string {
  if (ts == null || !Number.isFinite(ts) || ts <= 0) return '—'
  const d = new Date(ts * 1000)
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

/** 耗时毫秒 → 人话（`820ms` / `18s` / `1m20s` / `1h02m`）；null = 没跑完 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '—'
  if (ms < 1000) return `${Math.round(ms)}ms`
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const ss = s % 60
  if (m < 60) return `${m}m${pad(ss)}s`
  const h = Math.floor(m / 60)
  return `${h}h${pad(m % 60)}m`
}

/** 进行中的已耗时（秒级 now 来自轮询；时钟回拨/未开始一律 0） */
export function elapsedMs(startedAt: number | null | undefined, nowSecs: number): number {
  if (startedAt == null || !Number.isFinite(startedAt) || startedAt <= 0) return 0
  const ms = nowSecs * 1000 - startedAt * 1000
  return ms > 0 ? ms : 0
}

/** 全库累计队列计数（迁移前老行的口径）——只用于「历史累计（含迁移前）」折叠区 */
export interface LegacyQueueCounts {
  assetDone: number
  assetFailed: number
  assetPending: number
  groupDone: number
  groupFailed: number
  groupPending: number
}

export interface TaskReason {
  /** skip = 这条 task 没跑判定就跳过了（不是失败）；fail = 真失败 */
  kind: 'skip' | 'fail'
  /** 一句话人话（前缀已翻译） */
  text: string
  /** 原始 lastError（证据原文，界面折叠展示，不加工） */
  raw: string
}

/**
 * 跳过原因前缀（**唯一口径**，后端 `crates/svc-pipeline/src/skip_reason.rs` 的 `SKIP_PREFIXES`
 * 是同一份词表：改一处必须同步改另一处，`scripts/cmp_job_e2e*.{py,mjs}` 会抓不一致）。
 *
 * 跳过与成功同终态 `done`，界面只能靠前缀区分；所以**每条计入 run.skipped 的 task 都带原因**，
 * 且 `run.skipped == 明细里带跳过前缀的 task 数`（后端 e2e P5 硬断言）。
 * `unreadable:` 刻意不在表里：读不出是环境问题，必须显示成失败。
 */
/** i18n-D：跳过原因前缀表每次调用取词（label 是用户可见文案，不能在模块层固化） */
function skipPrefixes(): [string, string][] {
  return [
    // 更具体的前缀放前面（`not_image_media:` 不能被 `not_image:` 抢走）
    ['not_image_media:', tGlobal('不是图片（媒体类型不是 image）')],
    ['deleted:', tGlobal('资产已删除')],
    ['file_missing:', tGlobal('文件已失联（磁盘上找不到）')],
    ['not_image:', tGlobal('不是图片（文件魔数不是图像）')],
  ]
}

/**
 * task 的 `lastError` → 可读原因。
 *
 * 后端口径（`mark_skipped` / `skip_reason`）：跳过原因也写进 `lastError`，带
 * `not_image:` / `not_image_media:` / `file_missing:` / `deleted:` 前缀；没有前缀的 = 真失败原文。
 * 这里只翻译前缀，**不猜**失败原因（原文必须留着可核对）。
 */
export function taskReason(lastError: string | null | undefined): TaskReason | null {
  const raw = (lastError ?? '').trim()
  if (raw === '') return null
  for (const [prefix, label] of skipPrefixes()) {
    if (raw.startsWith(prefix)) {
      const detail = raw.slice(prefix.length).trim()
      return { kind: 'skip', text: detail ? tGlobal('{label}：{detail}', { label, detail }) : label, raw }
    }
  }
  return { kind: 'fail', text: raw, raw }
}

/** 步骤名：step 已是后端给的人话（`逐图判定 · 规则 F0_domain`），这里只做超长截断，不改写内容 */
export function stepText(task: ComplianceJobTaskView, max = 80): { short: string; full: string } {
  const full = task.step || `${task.statusLabel} · #${task.assetId}`
  return { short: full.length > max ? `${full.slice(0, max)}…` : full, full }
}

export type TaskWord = 'ok' | 'skip' | 'fail' | 'running' | 'wait'

export interface TaskResult {
  word: TaskWord
  label: string
  /** 该 task 是否有可展开的原始错误文本 */
  detail: string | null
}

/**
 * 该 task **现在**的原因（界面只显示这条）。
 *
 * 三条规则（CMP-JOB-FIX-1：原因行不能与状态打架）：
 * - `failed` → 原文照显示（真失败，证据必须可核对）；
 * - `done` + 跳过前缀 → 跳过原因（跳过与成功同终态，只靠前缀分）；
 * - `done` + 非跳过前缀 → **不显示**：成功就没有「为什么没结果」；留着只会渲染成
 *   「已完成 + 失败原因」（迁移前老行/上一版残留），那不是当前事实。
 */
export function taskReasonLine(task: ComplianceJobTaskView): TaskReason | null {
  const reason = taskReason(task.lastError)
  if (!reason) return null
  if (task.status === 'failed') return reason
  if (task.status === 'done') return reason.kind === 'skip' ? reason : null
  return null
}

/** task 结果 → 三态 + 跳过（跳过与成功同终态，必须靠 lastError 前缀区分，见后端 `mark_skipped`） */
export function taskResult(task: ComplianceJobTaskView): TaskResult {
  const reason = taskReasonLine(task)
  if (task.status === 'failed') {
    return { word: 'fail', label: tGlobal('失败'), detail: reason?.raw ?? null }
  }
  if (task.status === 'running') return { word: 'running', label: task.statusLabel || tGlobal('执行中'), detail: null }
  if (task.status === 'pending') return { word: 'wait', label: task.statusLabel || tGlobal('排队中'), detail: null }
  if (reason && reason.kind === 'skip') {
    return { word: 'skip', label: tGlobal('跳过'), detail: reason.raw }
  }
  return { word: 'ok', label: task.statusLabel || tGlobal('已完成'), detail: null }
}

export function taskResultClass(word: TaskWord): string {
  switch (word) {
    case 'ok':
      return 'border-line bg-panel-2 text-ink'
    case 'skip':
      return 'border-line bg-panel-2 text-ink-dim'
    case 'fail':
      return 'border-rose-600/60 bg-rose-600/10 text-rose-300'
    case 'running':
      return 'border-accent/60 bg-accent-soft text-ink'
    default:
      return 'border-line bg-panel-2 text-ink-dim'
  }
}

/** 明细分页口径：一页 100 条（后端 limit 上限 1000，60～100 是安全区，列表不会被撑爆） */
export const DETAIL_PAGE = 100

/** 分页区间文案：`1–100 / 320`（total 为 0 → `0 / 0`） */
export function rangeText(offset: number, limit: number, total: number): string {
  if (total <= 0) return '0 / 0'
  const from = Math.min(offset + 1, total)
  const to = Math.min(offset + limit, total)
  return `${from}–${to} / ${total}`
}
