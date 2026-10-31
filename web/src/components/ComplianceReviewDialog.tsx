/**
 * COMPLIANCE-UI3：人工改判/撤销对话框（合规页与检查器**共用同一个入口**）。
 *
 * 口径（用户裁决 2026-10-17）：
 * - 人的裁决只有两档：合格 / 不合格；「未判定」不是人的结论，是机器过程出错。
 * - 机器结论永不改写：对话框只看得到、不能编辑 machineResult/machineConfidence。
 * - 备注**必填**（服务端 400 review_reason_required）—— 没有理由的改判不可审计。
 * - 高置信度结论（confidence >= threshold）改判要显式勾选确认（服务端 400 review_not_low_confidence）。
 */
import { useEffect, useState } from 'react'
import { ApiError } from '../lib/api'
import { useI18n } from '../i18n'
import { PathLink } from './PathLink'
import { confidenceHint, confidenceText, needsForce, stateLabel } from '../lib/compliance'
import { pickI18nOr, usePackTextLocale, type PackText } from '../i18n/pickI18n'
import { Button } from './Button'

export interface ReviewTarget {
  assetId: number
  fileName: string
  absPath?: string | null
  /** null = 资产级改判（整张图一条结论） */
  ruleId?: string | null
  ruleName?: string | null
  /** 规则名原样 i18n 对象（i18n-F；包写对象形态时才有，渲染按当前语言取） */
  ruleNameI18n?: PackText
  /** 机器结论（只读展示） */
  machineResult: string
  machineConfidence: number | null
  /** 低置信度阈值（pack.report.low_confidence） */
  threshold: number
  /** 已有生效改判 → 对话框显示「撤销改判」入口 */
  hasReview?: boolean
  currentHuman?: 'pass' | 'fail' | null
}

export interface ReviewDone {
  /** 动作：改判（pass/fail）或撤销 */
  action: 'pass' | 'fail' | 'revoke'
  assetId: number
  ruleId: string | null
  /** 服务端回的资产生效档位（撤销时也可能返回） */
  assetState?: string | null
  assetStateLabel?: string | null
}

interface Props {
  target: ReviewTarget
  /** 提交实现（默认直连 api.complianceReviewCreate；测试可注入） */
  onSubmit: (req: {
    assetId: number
    ruleId: string | null
    humanStatus: 'pass' | 'fail'
    reason: string
    force: boolean
  }) => Promise<{ assetState?: string | null; assetStateLabel?: string | null }>
  onRevoke: (req: { assetId: number; ruleId: string | null; reason: string }) => Promise<{
    assetState?: string | null
    assetStateLabel?: string | null
  }>
  onClose: () => void
  onDone: (r: ReviewDone) => void
}

export function ComplianceReviewDialog({ target, onSubmit, onRevoke, onClose, onDone }: Props) {
  const { t } = useI18n()
  const locale = usePackTextLocale()
  const [human, setHuman] = useState<'pass' | 'fail'>(
    target.currentHuman === 'fail' ? 'pass' : target.currentHuman === 'pass' ? 'fail' : 'pass',
  )
  const [reason, setReason] = useState('')
  const [force, setForce] = useState(false)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  const [revoking, setRevoking] = useState(false)

  const mustForce = needsForce(target.machineConfidence, target.threshold)
  const ruleId = target.ruleId ?? null
  const reasonOk = reason.trim().length > 0
  const canSubmit = reasonOk && !busy && (!mustForce || force)

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        onClose()
      }
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [onClose])

  const submit = async () => {
    if (!canSubmit) return
    setBusy(true)
    setErr(null)
    try {
      const r = await onSubmit({
        assetId: target.assetId,
        ruleId,
        humanStatus: human,
        reason: reason.trim(),
        force,
      })
      onDone({
        action: human,
        assetId: target.assetId,
        ruleId,
        assetState: r.assetState ?? null,
        assetStateLabel: r.assetStateLabel ?? null,
      })
    } catch (e) {
      const code = e instanceof ApiError ? e.code : undefined
      if (code === 'review_reason_required') setErr(t('备注必填：写清改判依据（会进审计历史）'))
      else if (code === 'review_not_low_confidence') setErr(t('该结论置信度不低于阈值，需勾选「确认改判高置信度结论」'))
      else setErr(e instanceof Error ? e.message : String(e))
      setBusy(false)
    }
  }

  const revoke = async () => {
    setRevoking(true)
    setErr(null)
    try {
      const r = await onRevoke({ assetId: target.assetId, ruleId, reason: '' })
      onDone({
        action: 'revoke',
        assetId: target.assetId,
        ruleId,
        assetState: r.assetState ?? null,
        assetStateLabel: r.assetStateLabel ?? null,
      })
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
      setRevoking(false)
    }
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={t('人工改判')}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className="w-full max-w-lg rounded-lg border border-line bg-panel p-5 shadow-xl">
        <div className="mb-3 flex items-start justify-between gap-3">
          <div className="min-w-0">
            <h2 className="text-sm font-semibold">{t('人工复核 · ')}{ruleId ? t('规则级') : t('整张图')}</h2>
            <div className="mt-1 truncate text-xs text-ink-dim" title={target.absPath ?? target.fileName}>
              {target.fileName}
              {target.ruleName ? ` · ${pickI18nOr(target.ruleNameI18n, target.ruleName, locale)}` : ''}
            </div>
            {/* KI-10：改判前可直接打开文件所在目录核对原图（路径而不是裸文字） */}
            {target.absPath ? <PathLink path={target.absPath} mode="reveal" className="mt-1" /> : null}
          </div>
          <Button
            type="button"
            onClick={onClose}
            aria-label={t('关闭')}
            variant="icon"
          >
            ✕
          </Button>
        </div>

        {/* 机器结论（只读，不可编辑 —— 「机器永不改写」在界面上的体现） */}
        <div className="mb-4 rounded-md border border-line bg-panel-2 px-3 py-2 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-ink-dim">{t('机器结论')}</span>
            <span className="font-medium">{stateLabel(target.machineResult)}</span>
            <span className="text-ink-dim">· {confidenceText(target.machineConfidence)}</span>
          </div>
          <div className="mt-1 text-ink-dim">{confidenceHint(target.machineConfidence, target.threshold)}</div>
        </div>

        <div className="mb-3">
          <div className="mb-1.5 text-xs text-ink-dim">{t('改判为')}</div>
          <div className="flex gap-2">
            {(['pass', 'fail'] as const).map((v) => (
              // 选中态：accent-soft 底色 + aria-pressed（不再用 emerald/rose 自绘，静止态可辨由 Button 载体保证）
              <Button
                key={v}
                type="button"
                onClick={() => setHuman(v)}
                aria-pressed={human === v}
                variant="secondary"
                size="md"
                tone={v === 'fail' ? 'danger' : 'default'}
                active={human === v}
              >
                {v === 'pass' ? t('合格') : t('不合格')}
              </Button>
            ))}
          </div>
        </div>

        <label className="mb-1 block text-xs text-ink-dim" htmlFor="review-reason">
          {t('改判依据（必填，进审计历史）')}
        </label>
        <textarea
          id="review-reason"
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          rows={3}
          placeholder={t('例：主图背景是纯白，白边检测误报；已人工核对原图')}
          className="mb-3 w-full resize-none rounded-md border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
        />

        {mustForce && (
          <label className="mb-3 flex items-start gap-2 text-xs text-amber-300">
            <input
              type="checkbox"
              checked={force}
              onChange={(e) => setForce(e.target.checked)}
              className="mt-0.5"
            />
            {t('确认改判高置信度结论（机器把握很足，改判会覆盖它）')}
          </label>
        )}

        {err && <div className="mb-3 rounded-md border border-rose-600/60 bg-rose-600/10 px-2 py-1.5 text-xs text-rose-300">{err}</div>}

        <div className="flex items-center justify-between gap-2">
          <div>
            {target.hasReview && (
              <Button
                type="button"
                onClick={revoke}
                disabled={revoking}
                title={t('撤销后生效结论回到机器结论（改判行保留为历史）')}
                variant="secondary"
              >
                {revoking ? t('撤销中…') : t('撤销改判')}
              </Button>
            )}
          </div>
          <div className="flex gap-2">
            <Button
              type="button"
              onClick={onClose}
              variant="secondary"
            >
              {t('取消')}
            </Button>
            <Button
              type="button"
              onClick={submit}
              disabled={!canSubmit}
              title={reasonOk ? t('提交改判') : t('备注必填')}
              variant="primary"
            >
              {busy ? t('提交中…') : t('提交改判为{v}', { v: human === 'pass' ? t('合格') : t('不合格') })}
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}
