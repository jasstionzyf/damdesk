/**
 * DECouple-FE（卡 task）：**复核队列 —— 低频 Dialog**（原 `/review` 独立页的替代形态）。
 *
 * 背景与裁决（2026-10-31，权威）：图库/搜索/inspector 是**纯资产域**（固有属性 + 标签/色标/拒选/目录），
 * 零 job/判定概念。判定/处理的一切——查看、下钻、**改判**——都在任务中心闭环。
 * 原 `pages/Compliance.tsx`（1591 行，自带发起判定 + 结果网格 + 队列 + 列表四套东西）
 * 与任务中心 L2/L3 重复，故整体下线；它**唯一独有**的能力是「跨 job 的待人工看清单」，
 * 收进本 Dialog：入口只在任务中心工具栏（一个能力一个入口），深链 `?dialog=review-queue` 可分享。
 *
 * 口径（沿用 UI3 三条硬约束，改判仍走同一个 `ComplianceReviewDialog`）：
 * 1. 机器结论永不改写：行内同时显示 machineResult（+ 置信度）与生效结论 finalStatus；
 * 2. 人的裁决只有两档（合格 / 不合格）；「未判定」= 机器过程出错，人来兜底；
 * 3. 备注必填（服务端 400 review_reason_required）、高置信度改判要显式 force。
 */
import { useCallback, useEffect, useState } from 'react'

import { api, type ComplianceReviewItem } from '../lib/api'
import { confidenceText, isPluginReason, reasonText, stateLabel, tierClass } from '../lib/compliance'
import { useI18n } from '../i18n'
import { pickI18nOr, usePackTextLocale } from '../i18n/pickI18n'
import { useToast } from '../lib/toast'
import { Dialog } from '../lib/modal'
import { Button } from './Button'
import { ComplianceReviewDialog, type ReviewTarget } from './ComplianceReviewDialog'

type Scope = 'needs' | 'reviewed'

/** 生效结论徽章（与结果表同一套 tier 配色，避免两处语义漂） */
function StateBadge({ word, testid }: { word: string; testid?: string }) {
  return (
    <span
      data-testid={testid}
      className={`shrink-0 rounded border px-1.5 py-0.5 text-[11px] ${tierClass(word)}`}
    >
      {stateLabel(word)}
    </span>
  )
}

export function ReviewQueueDialog({
  open,
  onClose,
  onChanged,
}: {
  open: boolean
  onClose: () => void
  /** 改判落库后通知外部（任务中心据此刷新「待人工看」计数） */
  onChanged?: () => void
}) {
  const { t } = useI18n()
  const locale = usePackTextLocale()
  const { toast } = useToast()
  const [scope, setScope] = useState<Scope>('needs')
  const [rows, setRows] = useState<ComplianceReviewItem[]>([])
  const [total, setTotal] = useState(0)
  const [threshold, setThreshold] = useState(0.7)
  const [loading, setLoading] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  const [target, setTarget] = useState<ReviewTarget | null>(null)
  const [tick, setTick] = useState(0)

  useEffect(() => {
    if (!open) return
    let alive = true
    setLoading(true)
    setErr(null)
    api
      .complianceReview({
        lowConfidence: scope === 'needs' ? true : undefined,
        reviewed: scope === 'reviewed' ? true : undefined,
        sort: 'asset',
        limit: 200,
      })
      .then((r) => {
        if (!alive) return
        setRows(r.items)
        setTotal(r.total)
        setThreshold(r.threshold)
      })
      .catch((e) => {
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (alive) setLoading(false)
      })
    return () => {
      alive = false
    }
  }, [open, scope, tick])

  const openReview = useCallback((row: ComplianceReviewItem) => {
    setTarget({
      assetId: row.assetId,
      fileName: row.fileName,
      absPath: row.absPath,
      ruleId: row.ruleId,
      ruleName: row.ruleName ?? null,
      ruleNameI18n: row.ruleNameI18n,
      machineResult: row.machineResult,
      machineConfidence: row.machineConfidence,
      threshold,
      hasReview: row.humanReview != null,
      currentHuman: row.humanReview?.humanStatus ?? null,
    })
  }, [threshold])

  return (
    <>
      <Dialog
        open={open}
        onClose={onClose}
        title={t('待人工看')}
        subtitle={t(
          '这里只列机器没给出结论（过程出错）或置信度不足的条目。改判会留下依据，机器结论原样保留。',
        )}
        testId="review-queue-dialog"
        widthClass="max-w-3xl"
      >
        <div className="mb-3 flex flex-wrap items-center gap-2 text-xs" data-testid="review-queue-scope">
          {(['needs', 'reviewed'] as Scope[]).map((k) => (
            <button
              key={k}
              type="button"
              data-testid={`review-queue-scope-${k}`}
              aria-pressed={scope === k}
              onClick={() => setScope(k)}
              className={`rounded border px-2 py-0.5 ${
                scope === k
                  ? 'border-accent bg-accent-soft text-ink'
                  : 'border-line text-ink-dim hover:text-ink'
              }`}
            >
              {k === 'needs' ? t('待处理') : t('已改判')}
            </button>
          ))}
          <span className="text-ink-dim" data-testid="review-queue-total">
            {t('共 {n} 条', { n: total })}
          </span>
        </div>

        {err && (
          <p className="mb-2 break-all text-xs text-rose-300" data-testid="review-queue-error">
            {t('读取复核队列失败：{err}', { err })}
          </p>
        )}
        {loading && rows.length === 0 && (
          <p className="text-xs text-ink-dim" data-testid="review-queue-loading">
            {t('读取中…')}
          </p>
        )}
        {!loading && !err && rows.length === 0 && (
          <p className="rounded border border-line bg-panel-2 px-3 py-2 text-xs text-ink-dim" data-testid="review-queue-empty">
            {scope === 'needs' ? t('没有待人工看的条目。') : t('还没有改判记录。')}
          </p>
        )}

        <div className="space-y-1.5">
          {rows.map((r) => (
            <div
              key={`${r.assetId}-${r.ruleId ?? 'asset'}`}
              data-testid={`review-queue-row-${r.assetId}-${r.ruleId ?? 'asset'}`}
              className="flex items-start gap-2 rounded border border-line bg-panel-2 p-2 text-xs"
            >
              <img
                src={api.thumbUrl(r.assetId, 256)}
                alt=""
                loading="lazy"
                className="h-12 w-12 shrink-0 rounded object-cover"
              />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className="truncate font-medium" title={r.fileName}>
                    {r.fileName}
                  </span>
                  <span className="text-ink-dim">
                    {r.ruleNameI18n
                      ? pickI18nOr(r.ruleNameI18n, r.ruleName, locale)
                      : r.ruleName ?? t('整张图结论')}
                  </span>
                </div>
                <div className="mt-1 flex flex-wrap items-center gap-1.5 text-ink-dim">
                  <StateBadge word={r.machineResult} testid={`review-queue-machine-${r.assetId}`} />
                  <span>{confidenceText(r.machineConfidence)}</span>
                  {r.reviewed && r.humanReview && (
                    <>
                      <span>→</span>
                      <StateBadge word={r.finalStatus} testid={`review-queue-final-${r.assetId}`} />
                      <span className="truncate" title={r.humanReview.reason}>
                        {r.humanReview.reason}
                      </span>
                    </>
                  )}
                </div>
                {reasonText(r.reason) && (
                  <p className="mt-1 break-all text-[11px] text-amber-300" data-testid={`review-queue-reason-${r.assetId}-${r.ruleId ?? 'asset'}`}>
                    {reasonText(r.reason)}
                    {isPluginReason(r.reason) && (
                      <span className="ml-1.5 text-ink-dim" data-testid={`review-queue-source-plugin-${r.assetId}-${r.ruleId ?? 'asset'}`}>
                        {t('来源 检查能力')}
                      </span>
                    )}
                  </p>
                )}
              </div>
              <Button
                size="md"
                data-testid={`review-queue-action-${r.assetId}-${r.ruleId ?? 'asset'}`}
                onClick={() => openReview(r)}
              >
                {r.reviewed ? t('改判 / 撤销') : t('改判')}
              </Button>
            </div>
          ))}
        </div>
      </Dialog>

      {target && (
        <ComplianceReviewDialog
          target={target}
          onSubmit={async (req) => {
            const r = await api.complianceReviewCreate({
              assetId: req.assetId,
              ruleId: req.ruleId ?? undefined,
              humanStatus: req.humanStatus,
              reason: req.reason,
              force: req.force,
            })
            return { assetState: r.assetState, assetStateLabel: r.assetStateLabel }
          }}
          onRevoke={async (req) => {
            const r = await api.complianceReviewRevoke({
              assetId: req.assetId,
              ruleId: req.ruleId,
              reason: req.reason || undefined,
            })
            return { assetState: r.assetState, assetStateLabel: r.assetStateLabel }
          }}
          onClose={() => setTarget(null)}
          onDone={(d) => {
            setTarget(null)
            toast(
              'success',
              d.action === 'revoke' ? t('已撤销改判') : t('已记录改判'),
              d.assetStateLabel ?? '',
            )
            setTick((n) => n + 1)
            onChanged?.()
          }}
        />
      )}
    </>
  )
}
