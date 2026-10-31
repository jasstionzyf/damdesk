/**
 * DM-B2（KI-37）L2：**「这一次的结果」**（检查 / 处理两型的共用落地）。
 *
 * 对照 plan §2 的 L2 要求：
 *  * 检查类：判定分布卡（通过 / 未通过 / 待人工看，数字可点 → 过滤）+ 过滤 chip +
 *    结果表（列由 `result_columns` 声明，行前带缩略图 —— 判的是图就得看见图）；
 *  * 处理类：成功 / 失败 / 跳过 + 产物缩略图（勾选导出，PROC-RES-2 不回退）；
 *  * 出口：「看这批图 →」开 L3 图列表 Dialog；「只看待人工看 →」就地过滤（不再跳合规页丢上下文）。
 *
 * 硬边界（沿用 N12 / PROC-RES-2）：
 *  * 表格行数 == 接口行数（默认不筛；筛了会在计数里明说「已筛出 N」）；
 *  * 审核列与复核动作只有检查类才有 —— 处理类不给审核面；
 *  * `?job=<runId>` 深链仍可恢复（toast「看结果」跳的就是它）。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'

import { Button } from './Button'
import { JobImageListDialog, type JobImageItem, type ReviewFn } from './JobImageListDialog'
import { JobResultShell } from './JobResultShell'
import { ResultTable } from './ResultTable'
import {
  ApiError,
  api,
  artifactUrl,
  type ComplianceJobTaskView,
  type ComplianceResultsView,
} from '../lib/api'
import {
  VERDICT_LABELS,
  type VerdictStatus,
} from '../lib/pluginResult'
import { artifactBasename, artifactFiles, artifactPath, type ResultRowView } from '../lib/resultColumns'
import type { UnifiedJob } from '../lib/jobs'
import { tGlobal, useI18n } from '../i18n'

const RESULT_PAGE = 500

/** 一张图一个结论：同一张图多条规则行时 `fail > undetermined > pass`（与 `/jobs` 聚合同一口径） */
export function assetVerdictOf(statuses: VerdictStatus[]): VerdictStatus | null {
  if (statuses.includes('fail')) return 'fail'
  if (statuses.includes('undetermined')) return 'undetermined'
  if (statuses.includes('pass')) return 'pass'
  return null
}

function toResultRows(view: ComplianceResultsView) {
  return view.items.map((r) => ({
    assetId: r.assetId,
    fileName: r.fileName,
    ruleId: r.ruleId,
    judgementScope: r.judgementScope,
    result: r.result,
    verdict: r.verdict,
    reason: r.reason,
    fields: r.fields,
    artifacts: r.artifacts ?? null,
    ts: r.ts,
  }))
}

type VerdictFilter = 'all' | VerdictStatus

const VERDICT_CHIPS: { key: VerdictFilter; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'fail', label: VERDICT_LABELS.fail },
  { key: 'undetermined', label: VERDICT_LABELS.undetermined },
  { key: 'pass', label: VERDICT_LABELS.pass },
]

export interface JobResultViewProps {
  job: UnifiedJob
  onClose: () => void
  /** 结果变了（人工改判）→ 让 L1 重新取数，分布数字跟着更新 */
  onChanged?: () => void
}

/** 结果页副标题：方案/存放方式 · N 张 · 结果摘要（N 张 由 t() 出，单位随之翻译） */
function resultSubtitle(job: UnifiedJob): string {
  const head = job.subtitle ? `${job.subtitle} · ` : ''
  const count = job.count != null ? tGlobal('{n} 张 · ', { n: job.count }) : ''
  return `${head}${count}${job.summary}`
}

export function JobResultView({ job, onClose, onChanged }: JobResultViewProps) {
  const { t } = useI18n()
  const runId = job.runId
  const [view, setView] = useState<ComplianceResultsView | null>(null)
  const [failures, setFailures] = useState<ComplianceJobTaskView[]>([])
  const [runError, setRunError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  const [verdictFilter, setVerdictFilter] = useState<VerdictFilter>('all')
  const [selectedRows, setSelectedRows] = useState<ResultRowView[]>([])
  const [destDir, setDestDir] = useState('')
  const [exporting, setExporting] = useState(false)
  const [exportMsg, setExportMsg] = useState<string | null>(null)
  const [imagesOpen, setImagesOpen] = useState(false)
  /** DECouple-FE：按这次 job 的结果导出子集（检查类） */
  const [exportVerdict, setExportVerdict] = useState<'all' | 'pass' | 'fail' | 'undetermined'>('all')
  const [jobExporting, setJobExporting] = useState(false)
  const [jobExportMsg, setJobExportMsg] = useState<string | null>(null)
  const [jobDest, setJobDest] = useState('')

  const load = useCallback(async (id: number) => {
    setLoading(true)
    setErr(null)
    setFailures([])
    setRunError(null)
    try {
      // KI-40：结果行之外还要**任务行的失败原因**（失败的任务不产生结果行，
      // 只看结果表会「空空如也」——真机四倍超分失败就是在这里被吞的）。
      // 明细取不到不挡结果面（老版本后端没有这个接口也不至于白屏）。
      const [v, detail] = await Promise.all([
        api.complianceResults({ runId: id, limit: RESULT_PAGE }),
        api.complianceJob(id, { limit: 200 }).catch(() => null),
      ])
      setView(v)
      const tasks = detail?.tasks ?? []
      setFailures(tasks.filter((t) => t.status === 'failed' && (t.lastError ?? '').trim()))
      setRunError(detail?.run.error ?? null)
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
      setView(null)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    setVerdictFilter('all')
    if (runId == null) {
      setView(null)
      return
    }
    void load(runId)
  }, [runId, load])

  const kinds = view?.kinds ?? []
  const isJudge = kinds.includes('judge')
  const isProcess = kinds.includes('process')

  // 处理类：产物导出目录默认 = {dataDir}/exports/artifacts-<时间戳>（与图库导出同一根）；
  // 检查类：按结果导出子集的目录默认 = {dataDir}/exports/run-<runId>-<时间戳>
  useEffect(() => {
    if ((!isProcess && !isJudge) || (destDir && jobDest)) return
    let cancelled = false
    void api
      .appInfo()
      .then((info) => {
        if (cancelled || !info.dataDir) return
        const stamp = new Date().toISOString().slice(0, 16).replace(/[-:T]/g, '')
        if (isProcess && !destDir) setDestDir(`${info.dataDir}/exports/artifacts-${stamp}`)
        if (isJudge && !jobDest && runId != null) setJobDest(`${info.dataDir}/exports/run-${runId}-${stamp}`)
      })
      .catch(() => {
        /* 拿不到就让用户手填 */
      })
    return () => {
      cancelled = true
    }
  }, [isProcess, isJudge, destDir, jobDest, runId])

  /**
   * 失败原因：任务行 `lastError` ∪ 失败结果行 `reason`（同一资产只算一条）。
   *
   * KI40-FIX：只认任务行（`status==='failed'`）会让原因块恒空 —— 后端修好前跑出来的
   * run 是 `status=done` + 失败结果行；只认结果行又会漏掉「失败到没有结果行」的任务。
   * 两边取并集，按资产去重，文案就是结果行 `reason` 的原文（与结果表同源，不再兜底）。
   */
  const failureItems = useMemo(() => {
    const byAsset = new Map<number, string>()
    for (const t of failures) {
      const msg = (t.lastError ?? '').trim()
      if (msg) byAsset.set(t.assetId, msg)
    }
    for (const r of view?.items ?? []) {
      if (r.result !== 'failed') continue
      const msg = (r.reason ?? '').trim()
      if (msg) byAsset.set(r.assetId, msg)
    }
    return [...byAsset.values()]
  }, [failures, view])

  /** 失败原因去重计数（同原因 N 条并一行 ×N），最多 5 行 —— 原因是给「下一步做什么」用的，不是审计清单 */
  const failureLines = useMemo(() => {
    const m = new Map<string, number>()
    for (const msg of failureItems) {
      m.set(msg, (m.get(msg) ?? 0) + 1)
    }
    if (runError) m.set(runError, (m.get(runError) ?? 0))
    const lines = [...m.entries()].map(([msg, n]) => (n > 1 ? `${msg}（×${n}）` : msg))
    return lines.slice(0, 5)
  }, [failureItems, runError])

  const rows = useMemo(() => (view ? toResultRows(view) : []), [view])

  /** 每张图的结论（判定类；处理类恒空） */
  const assetVerdicts = useMemo(() => {
    const m = new Map<number, VerdictStatus>()
    const acc = new Map<number, VerdictStatus[]>()
    for (const r of view?.items ?? []) {
      const st = r.verdict?.status
      if (!st) continue
      const list = acc.get(r.assetId) ?? []
      list.push(st)
      acc.set(r.assetId, list)
    }
    for (const [id, list] of acc) {
      const v = assetVerdictOf(list)
      if (v) m.set(id, v)
    }
    return m
  }, [view])

  /** 本地看到的图级分布（接口行口径；与 `/jobs` 的聚合数字互为对照） */
  const localCounts = useMemo(() => {
    const c = { pass: 0, fail: 0, undetermined: 0 }
    for (const v of assetVerdicts.values()) c[v] += 1
    return c
  }, [assetVerdicts])

  const shownRows = useMemo(() => {
    if (verdictFilter === 'all') return rows
    return rows.filter((r) => assetVerdicts.get(r.assetId) === verdictFilter)
  }, [rows, verdictFilter, assetVerdicts])

  /** L3 的图列表：按图聚合（检查=结论 + 原因；处理=产物缩略图） */
  const imageItems = useMemo<JobImageItem[]>(() => {
    const seen = new Map<number, JobImageItem>()
    for (const r of shownRows) {
      const prev = seen.get(r.assetId)
      const verdict = assetVerdicts.get(r.assetId) ?? null
      if (prev) continue
      const art = artifactFiles(r)[0]
      const artPath = art ? artifactPath(art) : null
      seen.set(r.assetId, {
        assetId: r.assetId,
        fileName: r.fileName ?? null,
        verdict,
        reason: verdict && r.verdict?.status === verdict ? (r.verdict.reasonCode ?? r.reason ?? null) : (r.reason ?? null),
        thumbUrl: artPath ? artifactUrl(artPath) : api.thumbUrl(r.assetId, 256),
        fullUrl: artPath ? artifactUrl(artPath) : api.thumbUrl(r.assetId, 1024),
      })
    }
    return [...seen.values()]
  }, [shownRows, assetVerdicts])

  /** 人工改判（就地复核）：高置信度结论需要 force —— 用户既然点了改判就是要覆盖它 */
  const review: ReviewFn | undefined = isJudge
    ? async (assetId, humanStatus, reason) => {
        try {
          await api.complianceReviewCreate({ assetId, humanStatus, reason })
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e)
          if (!/low_confidence|force/i.test(msg)) throw e
          await api.complianceReviewCreate({ assetId, humanStatus, reason, force: true })
        }
        if (runId != null) await load(runId)
        onChanged?.()
      }
    : undefined

  const exportItems = useMemo(() => {
    const seen = new Set<string>()
    const items: { path: string; name: string }[] = []
    for (const row of selectedRows) {
      for (const a of artifactFiles(row)) {
        const p = artifactPath(a)
        if (p == null || seen.has(p)) continue
        seen.add(p)
        items.push({ path: p, name: artifactBasename(a) ?? p })
      }
    }
    return items
  }, [selectedRows])

  const exportSelected = async () => {
    if (!exportItems.length || !destDir.trim()) return
    setExporting(true)
    setExportMsg(null)
    try {
      const r = await api.complianceArtifactsExport({ items: exportItems, destDir: destDir.trim() })
      setExportMsg(
        r.missing.length
          ? t('已导出 {n} 个产物 → {dest}；{missing} 个没找到（文件可能被清理）', {
            n: r.copied,
            dest: r.dest,
            missing: r.missing.length,
          })
          : t('已导出 {n} 个产物 → {dest}', { n: r.copied, dest: r.dest }),
      )
    } catch (e) {
      setExportMsg(t('导出失败：{err}', { err: e instanceof Error ? e.message : String(e) }))
    } finally {
      setExporting(false)
    }
  }

  /** 本次要导出的张数（按所选档位；数字让用户点之前就知道会拿到多少） */
  const exportByResultCount = (() => {
    const c = job.verdicts ?? localCounts
    if (exportVerdict === 'all') return c.pass + c.fail + c.undetermined
    return c[exportVerdict]
  })()

  /**
   * 按这次 job 的结果导出（DECouple-BE 新端点）。
   * 端点还没上时后端回 404 —— 界面必须**如实说没导出**，不能像进程那样先报成功。
   */
  const exportByResult = async () => {
    if (runId == null || !jobDest.trim()) return
    setJobExporting(true)
    setJobExportMsg(null)
    try {
      const r = await api.complianceJobExport(runId, { verdict: exportVerdict, destDir: jobDest.trim() })
      setJobExportMsg(
        r.missing.length
          ? t('已导出 {n} 张 → {dest}；{missing} 张没找到（文件可能被清理）', {
            n: r.copied,
            dest: r.dest,
            missing: r.missing.length,
          })
          : t('已导出 {n} 张 → {dest}', { n: r.copied, dest: r.dest }),
      )
    } catch (e) {
      const missingEndpoint = e instanceof ApiError && (e.status === 404 || e.status === 501)
      setJobExportMsg(
        missingEndpoint
          ? t('这个版本的后端还没有「按结果导出」端点（DECouple-BE 卡），本次没有导出任何文件。')
          : t('导出失败：{err}', { err: e instanceof Error ? e.message : String(e) }),
      )
    } finally {
      setJobExporting(false)
    }
  }

  return (
    <JobResultShell
      title={t('这一次的结果 · {title}', { title: job.title })}
      subtitle={resultSubtitle(job)}
      onClose={onClose}
      testid="job-results"
      actions={
        <Button
          type="button"
          variant="secondary"
          onClick={() => setImagesOpen(true)}
          disabled={!imageItems.length}
          data-testid="job-images-open"
        >
          {t('看这批图（{n}）→', { n: imageItems.length })}
        </Button>
      }
    >
      {err && (
        <p className="break-all text-xs text-rose-300" data-testid="job-results-detail-error">
          {t('读取结果失败：{err}', { err })}
        </p>
      )}

      {/* KI-40：失败原因就地可见 —— 失败的任务不产生结果行，不在这里补原因就是「空空如也」 */}
      {(failureItems.length > 0 || runError) && (
        <div
          className="rounded-md border border-rose-600/50 bg-rose-600/5 p-2 text-xs text-rose-200"
          data-testid="job-failure-reasons"
        >
          <p className="font-semibold">
            {failureItems.length > 0
              ? t('失败 {n} 条，原因：', { n: failureItems.length })
              : t('这次执行中断，原因：')}
          </p>
          <ul className="mt-1 space-y-0.5">
            {failureLines.map((line, i) => (
              <li key={i} className="break-all pl-3 indent-[-0.6rem]">
                · {line}
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* 检查类：判定分布卡（数字就是人工最想要的那三个数，点一下即过滤） */}
      {isJudge && (
        <div className="flex flex-wrap items-stretch gap-2" data-testid="job-verdict-card">
          {/* 统计芯片（数字承载三档结论，点一下即按该档过滤）——不是按钮腔体 */}
          {(['pass', 'fail', 'undetermined'] as VerdictStatus[]).map((k) => {
            const value = job.verdicts ? job.verdicts[k] : localCounts[k]
            const active = verdictFilter === k
            return (
              <button
                key={k}
                type="button"
                onClick={() => setVerdictFilter(active ? 'all' : k)}
                className={`min-w-28 rounded border px-3 py-2 text-left ${
                  active ? 'border-accent/60 bg-accent/10' : 'border-line bg-panel-2 hover:border-accent/40'
                }`}
                data-testid={`job-verdict-${k}`}
                data-count={value}
              >
                <span className="block text-[11px] text-ink-dim">{t(VERDICT_LABELS[k])}</span>
                <span className="block text-lg font-semibold tabular-nums text-ink">
                  {value}
                  <span className="ml-0.5 text-[11px] font-normal text-ink-dim">{t('张')}</span>
                </span>
              </button>
            )
          })}
          <span className="flex-1" />
          <div className="flex flex-wrap items-center gap-1" data-testid="job-verdict-filter">
            {VERDICT_CHIPS.map((c) => (
              <Button
                key={c.key}
                type="button"
                variant="secondary"
                size="none"
                active={verdictFilter === c.key}
                className="h-6 px-2 text-[11px]"
                onClick={() => setVerdictFilter(c.key)}
                data-testid={`job-verdict-filter-${c.key}`}
              >
                {t(c.label)}
              </Button>
            ))}
            {/* 老探针/深链钩子：原来这条是「去复核未判定的图 →」跳走，现在就地过滤（回 L2 不丢上下文） */}
            <Button
              type="button"
              variant="link"
              size="none"
              className="text-[11px]"
              onClick={() => setVerdictFilter('undetermined')}
              data-testid="job-results-review-link"
            >
              {t('只看{v} →', { v: t(VERDICT_LABELS.undetermined) })}
            </Button>
          </div>
        </div>
      )}

      {/* DECouple-FE：按这次结果导出子集（过的一批 / 未过的一批 / 待人工看 / 全部）
          —— 与 F8 图库导出互不相干：图库导出只按资产域条件，不读任何判定字段 */}
      {isJudge && runId != null && (
        <div className="mt-2 flex flex-wrap items-center gap-2" data-testid="job-results-export-bar">
          <span className="text-[11px] text-ink-dim">{t('按这次结果导出')}</span>
          <select
            value={exportVerdict}
            onChange={(e) => setExportVerdict(e.target.value as typeof exportVerdict)}
            data-testid="job-results-export-verdict"
            className="rounded border border-line bg-panel px-1.5 py-0.5 text-[11px] text-ink focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
          >
            <option value="all">{t('本批全部')}</option>
            <option value="pass">{t('只导通过的')}</option>
            <option value="fail">{t('只导未通过的')}</option>
            <option value="undetermined">{t('只导待人工看的')}</option>
          </select>
          <label className="flex items-center gap-1 text-[11px] text-ink-dim">
            {t('导出到')}
            <input
              value={jobDest}
              onChange={(e) => setJobDest(e.target.value)}
              placeholder={t('后端机器上的绝对路径')}
              className="w-72 rounded border border-line bg-panel px-1.5 py-0.5 font-mono text-[11px] text-ink placeholder:text-ink-dim/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
              data-testid="job-results-export-dest"
            />
          </label>
          <Button
            type="button"
            variant="secondary"
            disabled={jobExporting || !jobDest.trim() || exportByResultCount === 0}
            onClick={() => void exportByResult()}
            data-testid="job-results-export-by-result"
          >
            {jobExporting ? t('导出中…') : t('导出（{n} 张）', { n: exportByResultCount })}
          </Button>
          {jobExportMsg && (
            <span className="break-all text-[11px] text-ink-dim" data-testid="job-results-export-result">
              {jobExportMsg}
            </span>
          )}
        </div>
      )}

      <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px] text-ink-dim">
        {isJudge ? (
          <span data-testid="job-results-no-audit" hidden>
            —
          </span>
        ) : (
          <span data-testid="job-results-no-audit">处理类结果没有审核列（只有执行结果与产物）</span>
        )}
        <span className="flex-1" />
        <span data-testid="job-results-shown">
          {t('显示 {shown} / {total} 行', { shown: shownRows.length, total: view?.total ?? 0 })}
        </span>
      </div>

      {isProcess && (
        <div className="mt-2 flex flex-wrap items-center gap-2" data-testid="job-results-export-bar">
          <span className="text-[11px] text-ink-dim">{t('已选 {n} 行', { n: selectedRows.length })}</span>
          <label className="flex items-center gap-1 text-[11px] text-ink-dim">
            {t('导出到')}
            <input
              value={destDir}
              onChange={(e) => setDestDir(e.target.value)}
              placeholder={t('后端机器上的绝对路径')}
              className="w-72 rounded border border-line bg-panel px-1.5 py-0.5 font-mono text-[11px] text-ink placeholder:text-ink-dim/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
              data-testid="job-results-export-dest"
            />
          </label>
          <Button
            type="button"
            variant="secondary"
            disabled={exporting || !exportItems.length || !destDir.trim()}
            onClick={() => void exportSelected()}
            data-testid="job-results-export-selected"
          >
            {exporting ? t('导出中…') : t(
              '导出选中产物（{n}）',
              { n: exportItems.length },
            )}
          </Button>
          {exportMsg && (
            <span className="break-all text-[11px] text-ink-dim" data-testid="job-results-export-result">
              {exportMsg}
            </span>
          )}
        </div>
      )}

      <div className="mt-2">
        <ResultTable
          rows={shownRows}
          columns={view?.columns ?? null}
          kinds={view?.kinds ?? null}
          total={view?.total}
          loading={loading}
          error={null}
          columnWarnings={view?.columnWarnings ?? []}
          selectable={isProcess}
          onSelectedRowsChange={setSelectedRows}
          thumbnails
          onThumbClick={() => setImagesOpen(true)}
          testid="job-result-table"
        />
      </div>

      <JobImageListDialog
        open={imagesOpen}
        title={t('这批图')}
        subtitle={`${job.title}${
              verdictFilter === 'all'
                ? ''
                : t(' · 只看{v}', { v: t(VERDICT_LABELS[verdictFilter as VerdictStatus]) })
            }`}
        items={imageItems}
        onClose={() => setImagesOpen(false)}
        onReview={review}
      />
    </JobResultShell>
  )
}
