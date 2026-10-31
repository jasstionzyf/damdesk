/**
 * DM-B2（KI-37）L3：**图级明细 Dialog**（缩略图网格 + 每图结论角标 + 点单图看大图与原因）。
 *
 * 用户裁决：需要看图时才弹（低频深度动作），关闭回 L2 不丢过滤与滚动位置 —— 所以它是
 * 独立 Dialog，不是新页面，也不改 `/tasks` 的选中态。
 *
 * 「就地复核」：点单图后可以直接改判（通过 / 未通过 + 原因），走既有
 * `/api/compliance/review` 人工改判（`onReview` 由 L2 注入）。这里不复用图库的检查器抽屉 ——
 * 那个抽屉绑着图库的选择集 URL 真源（`?ids=`/`?drawer=inspector`），搬到任务中心会把
 * 「浏览中的选择集」和「这次执行的结果集」搅成一份，改判口径也就跟着漂了。
 */
import { useEffect, useState } from 'react'

import { Button } from './Button'
import { Dialog } from '../lib/modal'
import { VERDICT_LABELS } from '../lib/pluginResult'
import type { VerdictStatus } from '../lib/pluginResult'
import { useI18n } from '../i18n'

export interface JobImageItem {
  assetId: number
  fileName: string | null
  /** 机器/生效结论（处理类没有结论 → null） */
  verdict: VerdictStatus | null
  /** 原因/证据一句话 */
  reason: string | null
  /** 网格缩略图 */
  thumbUrl: string
  /** 大图 */
  fullUrl: string
}

export type ReviewFn = (assetId: number, humanStatus: 'pass' | 'fail', reason: string) => Promise<void>

export interface JobImageListDialogProps {
  open: boolean
  title: string
  subtitle?: string | null
  items: JobImageItem[]
  onClose: () => void
  /** 给了才能就地改判（处理类结果没有合格/不合格 → 不传） */
  onReview?: ReviewFn
}

const BADGE: Record<VerdictStatus, string> = {
  pass: 'bg-emerald-500/85 text-emerald-950',
  fail: 'bg-rose-500/85 text-rose-950',
  undetermined: 'bg-amber-400/90 text-amber-950',
}

export function JobImageListDialog({
  open,
  title,
  subtitle,
  items,
  onClose,
  onReview,
}: JobImageListDialogProps) {
  const { t } = useI18n()
  const [selId, setSelId] = useState<number | null>(null)
  const [reviewStatus, setReviewStatus] = useState<'pass' | 'fail' | null>(null)
  const [reason, setReason] = useState('')
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)

  // 换一批图 / 关掉重开：回到「只网格」的干净态（不把上一次改判的输入带过去）
  useEffect(() => {
    if (!open) return
    setSelId(null)
    setReviewStatus(null)
    setReason('')
    setMsg(null)
    setBusy(false)
  }, [open, items])

  const sel = items.find((i) => i.assetId === selId) ?? null
  const submit = async () => {
    if (!sel || !reviewStatus || !onReview || !reason.trim()) return
    setBusy(true)
    setMsg(null)
    try {
      await onReview(sel.assetId, reviewStatus, reason.trim())
      setMsg(t('已改判为「{v}」', { v: t(VERDICT_LABELS[reviewStatus]) }))
      setReviewStatus(null)
      setReason('')
    } catch (e) {
      setMsg(t('改判失败：{err}', { err: e instanceof Error ? e.message : String(e) }))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      subtitle={subtitle ?? undefined}
      testId="job-images-dialog"
      widthClass="max-w-4xl"
    >
      {sel ? (
        <div className="flex flex-col gap-3" data-testid="job-image-detail">
          <div className="flex items-center justify-between gap-2">
            <span className="truncate text-xs text-ink" data-testid="job-image-detail-name">
              {sel.fileName ?? `#${sel.assetId}`}
            </span>
            <Button
              type="button"
              variant="secondary"
              size="none"
              className="h-6 px-2 text-[11px]"
              onClick={() => setSelId(null)}
              data-testid="job-image-detail-back"
            >
              {t('回到图列表')}
            </Button>
          </div>
          <div className="flex justify-center overflow-hidden rounded border border-line bg-panel-2">
            <img
              src={sel.fullUrl}
              alt={sel.fileName ?? `#${sel.assetId}`}
              className="max-h-[52vh] w-auto object-contain"
              data-testid="job-image-detail-img"
            />
          </div>
          <p className="text-xs text-ink-dim" data-testid="job-image-detail-verdict">
            {sel.verdict ? t(VERDICT_LABELS[sel.verdict]) : t('没有结论（处理类结果）')}
            {sel.reason ? ` · ${sel.reason}` : ''}
          </p>

          {onReview && (
            <div className="flex flex-wrap items-center gap-2 border-t border-line pt-3">
              <span className="text-[11px] text-ink-dim">{t('人工改判')}</span>
              <Button
                type="button"
                variant="secondary"
                size="none"
                className="h-6 px-2 text-[11px]"
                active={reviewStatus === 'pass'}
                onClick={() => setReviewStatus('pass')}
                data-testid="job-image-review-pass"
              >
                {t('通过')}
              </Button>
              <Button
                type="button"
                variant="secondary"
                size="none"
                tone="danger"
                className="h-6 px-2 text-[11px]"
                active={reviewStatus === 'fail'}
                onClick={() => setReviewStatus('fail')}
                data-testid="job-image-review-fail"
              >
                {t('未通过')}
              </Button>
              <input
                value={reason}
                onChange={(e) => setReason(e.target.value)}
                placeholder={t('改判原因（必填，留痕可追）')}
                className="w-64 rounded border border-line bg-panel px-1.5 py-0.5 text-[11px] text-ink placeholder:text-ink-dim/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
                data-testid="job-image-review-reason"
              />
              <Button
                type="button"
                variant="primary"
                size="none"
                className="h-6 px-2 text-[11px]"
                disabled={busy || !reviewStatus || !reason.trim()}
                onClick={() => void submit()}
                data-testid="job-image-review-submit"
              >
                {busy ? t('提交中…') : t('提交改判')}
              </Button>
              {msg && (
                <span className="text-[11px] text-ink-dim" data-testid="job-image-review-result">
                  {msg}
                </span>
              )}
            </div>
          )}
        </div>
      ) : (
        <div>
          {items.length === 0 ? (
            <p className="py-6 text-center text-xs text-ink-dim" data-testid="job-images-empty">
              {t('这一批没有可看的图（结果行里没有资产）。')}
            </p>
          ) : (
            <div className="grid grid-cols-2 gap-2 sm:grid-cols-4 lg:grid-cols-6" data-testid="job-images-grid">
              {items.map((it) => (
                /* 图卡片载体（整卡可点，与 AssetCard 同性质）——不是按钮腔体 */
                <button
                  key={it.assetId}
                  type="button"
                  onClick={() => setSelId(it.assetId)}
                  className="group relative overflow-hidden rounded border border-line bg-panel-2 text-left hover:border-accent/50"
                  data-testid={`job-image-cell-${it.assetId}`}
                >
                  <img
                    src={it.thumbUrl}
                    alt={it.fileName ?? `#${it.assetId}`}
                    loading="lazy"
                    className="aspect-square w-full object-cover"
                  />
                  {it.verdict && (
                    <span
                      className={`absolute left-1 top-1 rounded px-1 text-[10px] leading-4 ${BADGE[it.verdict]}`}
                      data-testid={`job-image-badge-${it.assetId}`}
                    >
                      {t(VERDICT_LABELS[it.verdict])}
                    </span>
                  )}
                  <span className="block truncate px-1 py-0.5 text-[10px] text-ink-dim" title={it.fileName ?? ''}>
                    {it.fileName ?? `#${it.assetId}`}
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}
    </Dialog>
  )
}
