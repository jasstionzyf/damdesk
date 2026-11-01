/**
 * N12：**检查 / 处理 共用的运行面板**（唯一一份实现，禁止两页各拼一套）。
 *
 * 面板只做三件事，顺序就是用户的思考顺序：
 *  1. **选方案**（用户嘴里的「方案」）—— 该动词下只有一条时默认选中，不让用户再点；
 *  2. **选图集**（作用范围）—— 选择集来自图库/搜索页带过来的 `?ids=`；处理没有「全部」兜底，
 *     避免一键改全库；
 *  3. **跑**（一个按钮，一个入口）—— 结果不在这里展示，跑完去任务中心看（一个能力一个入口）。
 *
 * 文案铁律（`scripts/ui_copy_guard.py` 守着）：不出现「插件 / job / 任务号 / verdict / 行业包」。
 * 方案的规则数、动作链、环境阻断原因都从方案数据与服务端 `pluginStatus` 直读，前端不猜。
 */
import { useMemo, type ReactNode } from 'react'

import { Button } from './Button'
import { useI18n } from '../i18n'
import type { SpecView } from '../lib/api'
import {
  VERB_LABEL,
  assetSetOptions,
  pickDefaultSpec,
  specRunBlockedReason,
  specSubtitle,
  specsForVerb,
  type SetKind,
  type Verb,
} from '../lib/specs'

export interface VerbRunPanelProps {
  verb: Verb
  specs: readonly SpecView[]
  specId: number | null
  onSpecChange: (specId: number) => void
  specsLoading?: boolean
  specsError?: string | null
  setKind: SetKind
  onSetKind: (kind: SetKind) => void
  /** 当前选择集张数（`?ids=` 带过来或页内选择） */
  selected: number
  /** 还没检查过的张数（只有检查类会用到） */
  pending?: number
  /** 图库总张数（只有检查类会用到） */
  total?: number
  running: boolean
  busy?: boolean
  error?: string | null
  /**
   * 判定前置失败的可操作指引（N18）：后端回 `code + hint + action` 时由调用方翻译成
   * 「一句人话 + 一个去处」。三种可补救场景：插件没装 / 方案指向已卸载插件 / 版本钉不上。
   */
  errorGuide?: { code: string; hint: string; href: string; label: string } | null
  /** 跑过之后的最近一次结果提示（含「去任务中心看结果」的链接位） */
  resultHint?: ReactNode
  /** 动词说明（一句话，解释这个按钮会做什么） */
  hint?: ReactNode
  onRun: () => void
  onReloadSpecs?: () => void
  /** 设置里的「方案目录」深链（新建/改方案在那里，不在这页） */
  catalogHref?: string
}

export function VerbRunPanel({
  verb,
  specs,
  specId,
  onSpecChange,
  specsLoading = false,
  specsError = null,
  setKind,
  onSetKind,
  selected,
  pending,
  total,
  running,
  busy = false,
  error = null,
  errorGuide = null,
  resultHint = null,
  hint = null,
  onRun,
  onReloadSpecs,
  catalogHref = '/settings#specs',
}: VerbRunPanelProps) {
  const { t } = useI18n()
  const usable = useMemo(() => specsForVerb(specs, verb), [specs, verb])
  const current = useMemo(() => {
    const byId = usable.find((s) => s.specId === specId) ?? null
    return byId ?? pickDefaultSpec(usable, verb, specId)
  }, [usable, specId, verb])
  const blocked = specRunBlockedReason(current)
  const setOpts = useMemo(
    () => assetSetOptions(verb, selected, pending, total),
    [verb, selected, pending, total],
  )
  const runnableSet = setKind === 'selection' ? selected > 0 : true
  const disabled = running || busy || !!blocked || !runnableSet

  return (
    <section
      className="mb-3 rounded-lg border border-line bg-panel p-3"
      data-testid={`${verb}-run-panel`}
    >
      <div className="flex flex-wrap items-end gap-3">
        {/* ---------- 1) 方案 ---------- */}
        <label className="flex min-w-0 flex-col gap-1 text-xs text-ink-dim">
          {t('{verb}用哪条方案', { verb: t(VERB_LABEL[verb]) })}
          {specsLoading ? (
            <span className="text-ink-dim" data-testid={`${verb}-specs-loading`}>
              {t('读取方案…')}
            </span>
          ) : usable.length === 0 ? (
            <span className="text-amber-400/90" data-testid={`${verb}-specs-empty`}>
              {t('还没有可用于{verb}的方案', { verb: t(VERB_LABEL[verb]) })}
            </span>
          ) : usable.length === 1 ? (
            // 只有一条 → 默认选中，用户不需要再点（也不给一个「只有一项」的下拉）
            <span className="text-sm text-ink" data-testid={`${verb}-spec-auto`}>
              {current?.name}
              <span className="ml-2 text-[11px] text-ink-dim">{t('（只此一条，已选中）')}</span>
            </span>
          ) : (
            <select
              value={current?.specId ?? ''}
              data-testid={`${verb}-spec-select`}
              onChange={(e) => onSpecChange(Number(e.target.value))}
              className="max-w-[18rem] rounded border border-line bg-panel px-2 py-1 text-ink outline-none"
            >
              <option value="" disabled>
                {t('选一条方案…')}
              </option>
              {usable.map((s) => (
                <option key={s.specId} value={s.specId}>
                  {s.name}
                </option>
              ))}
            </select>
          )}
          {current && (
            <span className="text-[11px] text-ink-dim" data-testid={`${verb}-spec-subtitle`}>
              {specSubtitle(current)}
              {current.pluginStatus.version && t(' · 版本 {v}', { v: current.pluginStatus.version })}
            </span>
          )}
        </label>

        {/* ---------- 2) 图集 ---------- */}
        <label className="flex flex-col gap-1 text-xs text-ink-dim">
          {verb === 'artifact' ? t('处理哪一批') : t('检查哪一批')}
          {setOpts.length === 1 ? (
            <span className="py-1 text-sm text-ink" data-testid={`${verb}-set-fixed`}>
              {setOpts[0].label}
            </span>
          ) : (
            <select
              value={setKind}
              data-testid={`${verb}-set-select`}
              onChange={(e) => onSetKind(e.target.value as SetKind)}
              className="rounded border border-line bg-panel px-2 py-1 text-ink outline-none"
            >
              {setOpts.map((o) => (
                <option key={o.kind} value={o.kind} disabled={o.kind === 'selection' && selected === 0}>
                  {o.label}
                  {o.count != null && o.kind !== 'selection' ? t('（{n} 张）', { n: o.count }) : ''}
                </option>
              ))}
            </select>
          )}
        </label>

        <span className="flex-1" />

        {/* ---------- 3) 跑 ---------- */}
        <Button
          type="button"
          variant="primary"
          disabled={disabled}
          onClick={onRun}
          title={blocked ?? (runnableSet ? undefined : t('先在图库选中要处理的图片'))}
          data-testid={`${verb}-run`}
        >
          {running
            ? t('{verb}中…', { verb: t(VERB_LABEL[verb]) })
            : t('开始{verb}', { verb: t(VERB_LABEL[verb]) })}
        </Button>
        {onReloadSpecs && (
          <Button
            type="button"
            variant="secondary"
            disabled={specsLoading}
            onClick={onReloadSpecs}
            data-testid={`${verb}-specs-reload`}
          >
            {t('刷新方案')}
          </Button>
        )}
      </div>

      {hint && (
        <p className="mt-2 text-[11px] leading-5 text-ink-dim" data-testid={`${verb}-hint`}>
          {hint}
        </p>
      )}

      {blocked && usable.length > 0 && (
        <p className="mt-2 text-[11px] text-amber-400/90" data-testid={`${verb}-spec-blocked`}>
          {blocked}
        </p>
      )}

      {usable.length === 0 && !specsLoading && (
        <p className="mt-2 text-[11px] leading-5 text-ink-dim" data-testid={`${verb}-specs-empty-hint`}>
          {t('方案 = 要交付成的样子（选哪些规则 / 阈值 / 动作链 / 输出）。到')}{' '}
          <a href={catalogHref} className="text-accent hover:underline">
            {t('设置 › 方案目录')}
          </a>{' '}
          {t('里新建一条，这一页只负责{verb}。', { verb: t(VERB_LABEL[verb]) })}
        </p>
      )}

      {specsError && (
        <p className="mt-2 break-all text-[11px] text-rose-300" data-testid={`${verb}-specs-error`}>
          {t('读取方案失败：{err}', { err: specsError })}
        </p>
      )}

      {error && (
        <p className="mt-2 break-all text-[11px] text-rose-300" data-testid={`${verb}-run-error`}>
          {error}
        </p>
      )}

      {errorGuide && (
        <div
          className="mt-2 flex flex-wrap items-center gap-2 rounded border border-amber-600/60 bg-amber-600/10 px-3 py-2 text-[11px] text-amber-200"
          data-testid={`${verb}-run-guide`}
        >
          <span className="leading-5" data-testid={`${verb}-run-guide-hint`}>
            {errorGuide.hint}
          </span>
          <a
            href={errorGuide.href}
            className="rounded border border-amber-500/60 px-2 py-0.5 text-amber-100 hover:bg-amber-500/20"
            data-testid={`${verb}-run-guide-action`}
          >
            {errorGuide.label} →
          </a>
          {errorGuide.code ? (
            <code className="text-[10px] text-amber-300/70" data-testid={`${verb}-run-guide-code`}>
              {errorGuide.code}
            </code>
          ) : null}
        </div>
      )}

      {resultHint && (
        <p className="mt-2 text-[11px] leading-5" data-testid={`${verb}-result-hint`}>
          {resultHint}
        </p>
      )}
    </section>
  )
}
