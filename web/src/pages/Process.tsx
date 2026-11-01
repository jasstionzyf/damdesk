/**
 * N12：**处理**页（用户视角三名词之一：图集 → 方案 → 结果）。
 *
 * 一句话：把选中的图变成方案描述的样子。页面上只有三样东西 —— 图集、方案、跑。
 * 没有「插件安装」「任务号」「执行明细」这类内部概念：装/卸/改方案在设置 › 方案目录，
 * 跑完的结果在任务中心（一个能力只归一个区域，违反即错 —— 见设计 §8 第 5 条）。
 *
 * 请求走**宿主唯一的执行入口** `POST /api/jobs`（N1 定名：设计口径 `specId` 引用方案）。
 * 前端不解释动作链、不改写参数：动作链与参数都在方案里，执行由宿主按方案编排。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'

import { VerbRunPanel } from '../components/VerbRunPanel'
import { Button } from '../components/Button'
import { api, type SpecView } from '../lib/api'
import { useToast } from '../lib/toast'
import { impactLine, useConfirm, verbCount } from '../lib/confirm'
import { useI18n } from '../i18n'
import {
  actionChainText,
  chainAffectsBody,
  parseIds,
  pickDefaultSpec,
  specVerb,
  specsForVerb,
  type SetKind,
} from '../lib/specs'

export function Process() {
  const [params] = useSearchParams()
  const ids = useMemo(() => parseIds(params.get('ids')), [params])
  const [specs, setSpecs] = useState<SpecView[]>([])
  const [specsLoading, setSpecsLoading] = useState(true)
  const [specsError, setSpecsError] = useState<string | null>(null)
  const [specId, setSpecId] = useState<number | null>(null)
  const [setKind, setSetKind] = useState<SetKind>('selection')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [lastRunId, setLastRunId] = useState<number | null>(null)
  const { toast } = useToast()
  const confirm = useConfirm()
  const { t } = useI18n()

  const loadSpecs = useCallback(async () => {
    setSpecsLoading(true)
    try {
      const r = await api.specs()
      setSpecs(r.items)
      setSpecsError(null)
      // 只有一条 → 直接选中（用户不用再点；多条则等用户选，不猜哪条是「他想要的」）
      setSpecId((cur) => pickDefaultSpec(r.items, 'artifact', cur)?.specId ?? cur)
    } catch (e) {
      setSpecsError(e instanceof Error ? e.message : String(e))
    } finally {
      setSpecsLoading(false)
    }
  }, [])

  useEffect(() => {
    void loadSpecs()
  }, [loadSpecs])

  const current = useMemo(
    () => specsForVerb(specs, 'artifact').find((s) => s.specId === specId) ?? null,
    [specs, specId],
  )

  const run = async () => {
    if (!current || ids.length === 0) return
    // 处理会改写资产的工作版本（原图不变）—— 动作链里有改本体的动作时二次确认
    if (chainAffectsBody(current)) {
      const ok = await confirm({
        title: t('按「{name}」处理这批图？', { name: current.name }),
        body: t('动作链：{chain}。结果写成这批图的新版本，原图保留、可回退；图库之外的文件不动。', { chain: actionChainText(current) }),
        impacts: [impactLine(t('处理'), ids.length, t('张')) ?? ''].filter(Boolean),
        recoverable: t('可回退（原图保留）'),
        confirmLabel: verbCount(t('处理'), ids.length, t('张')),
      })
      if (!ok) return
    }
    setRunning(true)
    setError(null)
    try {
      const r = await api.startJob({ specId: current.specId, assetIds: ids })
      setLastRunId(r.runId ?? null)
      toast('success', t('已开始处理'), t('{n} 张 · 方案「{name}」；进度与结果在任务中心', { n: ids.length, name: current.name }))
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setError(msg)
      toast('error', t('没能开始处理'), msg)
    } finally {
      setRunning(false)
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4" data-testid="process-page">
      <header className="mb-3 flex flex-wrap items-center gap-3">
        <h1 className="text-base font-semibold text-ink">{t('处理')}</h1>
        <span className="text-xs text-ink-dim">
          {t('把选中的图变成方案描述的样子（原图保留，结果写成新版本）')}
        </span>
        <span className="flex-1" />
        <Link to="/settings#specs" className="text-xs text-accent hover:underline">
          {t('方案目录 →')}
        </Link>
      </header>

      <VerbRunPanel
        verb="artifact"
        specs={specs}
        specId={specId}
        onSpecChange={setSpecId}
        specsLoading={specsLoading}
        specsError={specsError}
        setKind={setKind}
        onSetKind={setSetKind}
        selected={ids.length}
        running={running}
        error={error}
        onRun={() => void run()}
        onReloadSpecs={() => void loadSpecs()}
        hint={
          <>
            {t('处理只作用于**图集中选中的这批图**：先在')}
            <Link to="/" className="mx-1 text-accent hover:underline">
              {t('图库')}
            </Link>
            {t('里选好，再回来（本页不做第二套选图界面）。')}
          </>
        }
        resultHint={
          lastRunId != null ? (
            <>
              {t('这一次的结果已进任务中心：')}
              <Link to={`/tasks?job=${lastRunId}`} className="ml-1 text-accent hover:underline">
                {t('看结果 →')}
              </Link>
            </>
          ) : null
        }
      />

      {current && (
        <section className="mb-3 rounded-lg border border-line bg-panel p-3" data-testid="process-chain">
          <h2 className="text-sm font-semibold">{t('这条方案会做什么')}</h2>
          <ol className="mt-2 space-y-1 text-xs text-ink-dim" data-testid="process-chain-steps">
            {current.actions.map((a, i) => (
              <li key={`${a.action}-${i}`} className="flex flex-wrap items-center gap-2">
                <span className="rounded bg-panel-2 px-1.5 py-0.5 tabular-nums text-ink-dim">{i + 1}</span>
                <span className="text-ink">{a.action}</span>
                {a.params && Object.keys(a.params).length > 0 && (
                  <span className="break-all font-mono text-[11px] text-ink-dim">
                    {JSON.stringify(a.params)}
                  </span>
                )}
                {a.when != null && <span className="text-[11px] text-amber-400/90">{t('条件执行')}</span>}
              </li>
            ))}
            {current.actions.length === 0 && <li>{t('（这条方案没有动作链）')}</li>}
          </ol>
          <p className="mt-2 text-[11px] leading-5 text-ink-dim">
            {t('顺序即执行顺序；条件是「满足才做这一到步」。参数与动作都来自方案，本页不改写。')}
          </p>
        </section>
      )}

      {ids.length === 0 && (
        <section
          className="rounded-lg border border-dashed border-line px-3 py-4 text-xs text-ink-dim"
          data-testid="process-empty-set"
        >
          {t('还没有选图。到')}
          <Link to="/" className="mx-1 text-accent hover:underline">
            {t('图库')}
          </Link>
          {t('里选好图片，点「处理」把这一批带过来。')}
        </section>
      )}

      {specsForVerb(specs, 'artifact').length === 0 && !specsLoading && (
        <section className="text-xs text-ink-dim" data-testid="process-no-specs-catalog">
          <Button type="button" variant="link" size="none" onClick={() => void loadSpecs()}>
            {t('重新读取方案')}
          </Button>
          <span className="ml-2">
            {t('处理类方案 = 一列动作（例如缩图 → 换格式 → 压到指定体积）。若一条都没有，先在设置 ›方案目录里建一条。')}
          </span>
        </section>
      )}

      {/* 诊断位：方案分类自检（排查「为什么这条方案没出现在处理页」用；不占用户视线） */}
      {specs.length > 0 && (
        <details className="mt-3 text-[11px] text-ink-dim" data-testid="process-specs-diag">
          <summary className="cursor-pointer select-none">
            {t('方案清单（{n} 条，其中可处理 {m} 条）', {
              n: specs.length,
              m: specs.filter((s) => specVerb(s) === 'artifact').length,
            })}
          </summary>
          <ul className="mt-1 space-y-0.5">
            {specs.map((s) => (
              <li key={s.specId}>
                {s.name} · {specVerb(s) === 'artifact' ? t('处理') : t('检查')} ·{' '}
                {s.enabled === 0 ? t('已停用') : s.pluginStatus.installed ? t('可用') : t('能力未安装')}
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  )
}
