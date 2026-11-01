/**
 * KI35-P3（t_06bb84bd）：能力面板 —— 图库/搜索选择集「送去处理 →」的落点
 * （全局 Dialog，`?dialog=artifact&ids=1,2,3` 深链直达，与导出同一套浮层原语）。
 *
 * 三步一次性表单（§9.2.1 Dialog 判据：≤6 字段、一次性动作、无回看历史 —— 历史归「处理记录」页）：
 *  1. 能力：列出已装能力（能力名 + 一句话描述），动词词（检查/处理）由 manifest outputs 决定；
 *  2. 方案：该能力下的方案，分「官方默认」（包里带来的）与「我的方案」（自建）；
 *  3. 范围：当前选中 / 当前筛选结果（页面寄存的同一份条件）/ 全部图库（只给纯判定能力）。
 *
 * 确认 = POST /api/jobs（能力面板不发别的请求），随后去「处理记录」页看这一次的进度。
 * 文案红线：名词只有 能力 / 方案 / 处理记录，不出现 插件/规格/任务/job/verdict/manifest/行业包。
 */
import { useEffect, useMemo, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'

import { Button } from './Button'
import { parseIdsParam } from './ExportDialog'
import { api, type SpecView } from '../lib/api'
import { useCapabilityNav, type CapabilityNavItem } from '../lib/capabilityNav'
import { Dialog, useOverlay } from '../lib/modal'
import {
  FILTER_IDS_CAP,
  allowAllRange,
  capabilityVerbWord,
  loadProcessFilterScope,
  planGroups,
  rangeOptions,
  specsDerivedCapabilities,
  usablePlanCount,
  type ProcessFilterScope,
  type ProcessRange,
} from '../lib/sendToProcess'
import { specRunBlockedReason, specSubtitle, specVerb } from '../lib/specs'
import { useToast } from '../lib/toast'
import { tGlobal, useI18n } from '../i18n'

/** 筛选范围回读成执行集合：分页拉全（`/api/library/assets` 与计数同一份条件），超上限拒发 */
async function resolveFilterIds(
  scope: ProcessFilterScope,
  total: number | null,
): Promise<number[]> {
  if (total != null && total > FILTER_IDS_CAP) {
    throw new Error(tGlobal('筛选结果 {n} 张，超过单次上限 {cap} 张 —— 请缩小范围或分批', { n: total, cap: FILTER_IDS_CAP }))
  }
  const out: number[] = []
  let offset = 0
  // total 未知时最多翻 CAP/500 页，防住「读数没回来就无限拉」
  while (out.length < (total ?? FILTER_IDS_CAP)) {
    const r = await api.assets({ ...scope.filter, limit: 500, offset })
    if (r.items.length === 0) break
    for (const a of r.items) out.push(a.assetId)
    offset += r.items.length
    if (offset >= r.total) break
  }
  return out
}

export function SendToProcessDialog() {
  const { t } = useI18n()
  const ov = useOverlay('dialog', 'artifact', ['ids'])
  const [params] = useSearchParams()
  const navigate = useNavigate()
  const { toast } = useToast()

  const ids = useMemo(() => parseIdsParam(params.get('ids')), [params])
  const { items: packItems, loaded: capsLoaded } = useCapabilityNav()

  // 三步的选择态；面板关闭即复位（下次打开不带旧上下文）
  const [capId, setCapId] = useState<string | null>(null)
  const [specId, setSpecId] = useState<number | null>(null)
  const [range, setRange] = useState<ProcessRange>('selection')
  const [specs, setSpecs] = useState<SpecView[] | null>(null)
  const [specsError, setSpecsError] = useState<string | null>(null)
  const [allSpecs, setAllSpecs] = useState<SpecView[] | null>(null)
  const [filterTotal, setFilterTotal] = useState<number | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // 打开时读一遍全量方案：① 给能力清单补集（fresh 库上注册表还没有内置包的行），
  // ② 顺带触发后端 seeding（老库首次访问惰性补默认方案 —— 面板因此总能列出官方默认）。
  useEffect(() => {
    if (!ov.open) return
    let alive = true
    setAllSpecs(null)
    api
      .specs()
      .then((r) => {
        if (alive) setAllSpecs(r.items)
      })
      .catch(() => {
        // 读不到 ≠ 没有：补集为空（面板退回注册表口径），不阻塞
        if (alive) setAllSpecs([])
      })
    return () => {
      alive = false
    }
  }, [ov.open])

  // 能力清单 = 注册表 ∪ 方案派生补集（注册表行优先，outputs 来自 manifest）
  const capabilities = useMemo(() => {
    if (!allSpecs) return packItems
    const known = new Set(packItems.map((c) => c.id))
    return [...packItems, ...specsDerivedCapabilities(allSpecs, known)]
  }, [packItems, allSpecs])
  // 两边都读到 0 才给安装引导（单边没回来不闪空态）
  const capsReady = capsLoaded && allSpecs != null

  const filterScope = useMemo(() => (ov.open ? loadProcessFilterScope() : null), [ov.open])
  const cap = capabilities.find((c) => c.id === capId) ?? null

  // 关闭 → 复位全部选择态（深链重新打开从第一步开始，不猜上一次的上下文）
  useEffect(() => {
    if (!ov.open) {
      setCapId(null)
      setSpecId(null)
      setRange('selection')
      setSpecs(null)
      setSpecsError(null)
      setFilterTotal(null)
      setError(null)
    }
  }, [ov.open])

  // 选中能力 → 拉它下面的方案；换能力时清掉方案选择
  useEffect(() => {
    if (!ov.open || !capId) return
    let alive = true
    setSpecs(null)
    setSpecsError(null)
    setSpecId(null)
    api
      .specs(capId)
      .then((r) => {
        if (alive) setSpecs(r.items)
      })
      .catch((e) => {
        if (alive) setSpecsError(e instanceof Error ? e.message : String(e))
      })
    return () => {
      alive = false
    }
  }, [ov.open, capId])

  // 「当前筛选结果」的读数与执行用同一份条件（DT4 教训：预览和执行各算一份必打架）
  useEffect(() => {
    if (!ov.open || !filterScope) return
    let alive = true
    setFilterTotal(null)
    api
      .assets({ ...filterScope.filter, limit: 1 })
      .then((r) => {
        if (alive) setFilterTotal(r.total)
      })
      .catch(() => {
        // 读数失败 ≠ 范围不可用：执行时再按同一份条件回读，这里只少显示一个数字
      })
    return () => {
      alive = false
    }
  }, [ov.open, filterScope])

  const groups = useMemo(() => planGroups(specs ?? []), [specs])
  // 只有一条可用方案 → 直接选上（用户少点一次；多条必须显式选，specs.ts pickDefaultSpec 同口径）
  useEffect(() => {
    if (specs == null || specId != null) return
    const n = usablePlanCount(planGroups(specs))
    if (n === 1) {
      const only = [...planGroups(specs).official, ...planGroups(specs).mine][0]
      setSpecId(only.specId)
    }
  }, [specs, specId])

  const selectedSpec = useMemo(
    () => (specs ?? []).find((s) => s.specId === specId) ?? null,
    [specs, specId],
  )
  // 范围选项随能力/寄存上下文变：处理类不给「全部」；换能力后越界的档位落回「当前选中」
  const ranges = useMemo(
    () =>
      rangeOptions({
        selected: ids.length,
        filterScope,
        filterTotal,
        outputs: cap?.outputs ?? null,
      }),
    [ids.length, filterScope, filterTotal, cap],
  )
  useEffect(() => {
    if (!ranges.some((r) => r.kind === range)) setRange('selection')
  }, [ranges, range])

  // 确认按钮的动词：选中的方案有动作链 = 处理，没有 = 检查（数据说话，与 outputs 双保险）
  const verb = selectedSpec ? (specVerb(selectedSpec) === 'artifact' ? t('处理') : t('检查')) : null
  const canSubmit =
    !submitting && selectedSpec != null && !(range === 'selection' && ids.length === 0)

  async function submit() {
    if (!selectedSpec || !verb || !cap) return
    setError(null)
    try {
      let assetIds: number[] | undefined
      let all: boolean | undefined
      let scopeDesc = ''
      if (range === 'selection') {
        if (ids.length === 0) return
        assetIds = ids
        scopeDesc = t('选中 {n} 张', { n: ids.length })
      } else if (range === 'filter') {
        if (!filterScope) return
        setSubmitting(true)
        assetIds = await resolveFilterIds(filterScope, filterTotal)
        if (assetIds.length === 0) {
          toast('warning', t('筛选结果里没有可处理的图'), t('换个条件，或回图库直接选图'))
          setSubmitting(false)
          return
        }
        scopeDesc = t('筛选结果 {n} 张', { n: assetIds.length })
      } else {
        // 「全部」只可能出现在纯判定能力的选项里（rangeOptions 已门禁；这里再钉一道）
        if (!allowAllRange(cap.outputs)) return
        all = true
        scopeDesc = t('全部图库')
      }
      setSubmitting(true)
      const r = await api.startJob({ specId: selectedSpec.specId, assetIds, all })
      ov.closeOverlay()
      toast('success', t('已开始{v}', { v: verb }), t('{scope} · 方案「{name}」；进度与结果在「处理记录」', { scope: scopeDesc, name: selectedSpec.name }))
      navigate(`/tasks?job=${r.runId}`)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setSubmitting(false)
    }
  }

  if (!ov.open) return null

  return (
    <Dialog
      open
      title={`${t('送去处理')}${ids.length > 0 ? t('（选中的 {n} 张）', { n: ids.length }) : ''}`}
      subtitle={t('选一个能力，再选一条方案；检查类回答「这些图能不能用」，处理类把图变成你要的样子。进度与结果在「处理记录」页看。')}
      onClose={ov.closeOverlay}
      testId="send-process-dialog"
      footer={
        <>
          <Button type="button" variant="secondary" onClick={ov.closeOverlay} disabled={submitting}>
            {t('取消')}
          </Button>
          <Button
            type="submit"
            form="send-process-form"
            disabled={!canSubmit}
            title={selectedSpec ? '' : t('先选一条方案')}
          >
            {submitting ? t('正在提交…') : verb ? t('开始{v}', { v: verb }) : t('开始')}
          </Button>
        </>
      }
    >
      <form
        id="send-process-form"
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        {/* ---------- 1) 能力 ---------- */}
        <fieldset className="flex flex-col gap-1.5" data-testid="send-process-caps">
          <legend className="mb-1 text-xs font-semibold tracking-wide text-ink-dim">{t('能力')}</legend>
          {!capsReady && <p className="text-xs text-ink-dim">{t('正在读取已装的能力…')}</p>}
          {capsReady && capabilities.length === 0 && (
            <p className="text-xs leading-5 text-ink-dim" data-testid="send-process-caps-empty">
              {t('还没有可用的处理功能，去')}
              <Link to="/settings?drawer=packs" className="mx-0.5 text-accent hover:underline">
                {t('设置')}
              </Link>
              {t('安装后再回来。')}
            </p>
          )}
          {capabilities.map((c) => (
            <CapabilityRow
              key={c.id}
              cap={c}
              checked={capId === c.id}
              onPick={() => setCapId(c.id)}
            />
          ))}
        </fieldset>

        {/* ---------- 2) 方案 ---------- */}
        {cap && (
          <fieldset className="flex flex-col gap-1.5" data-testid="send-process-plans">
            <legend className="mb-1 text-xs font-semibold tracking-wide text-ink-dim">
              {t('方案（{label}）', { label: cap.label })}
            </legend>
            {specs == null && !specsError && <p className="text-xs text-ink-dim">{t('读取方案…')}</p>}
            {specsError && <p className="text-xs text-rose-300">{t('读取方案失败：{err}', { err: specsError })}</p>}
            {specs != null && usablePlanCount(groups) === 0 && (
              <p className="text-xs leading-5 text-ink-dim" data-testid="send-process-plans-empty">
                {t('这个能力还没有可用的方案，去')}
                <Link to={cap.to} className="mx-0.5 text-accent hover:underline">
                  {t('能力页')}
                </Link>
                {t('建一条再回来。')}
              </p>
            )}
            <PlanGroup
              title={t('官方默认')}
              hint={t('装包时自带的')}
              list={groups.official}
              specId={specId}
              onPick={setSpecId}
            />
            <PlanGroup
              title={t('我的方案')}
              hint={t('你自己建的')}
              list={groups.mine}
              specId={specId}
              onPick={setSpecId}
            />
          </fieldset>
        )}

        {/* ---------- 3) 范围 ---------- */}
        <fieldset className="flex flex-col gap-1.5" data-testid="send-process-ranges">
          <legend className="mb-1 text-xs font-semibold tracking-wide text-ink-dim">{t('范围')}</legend>
          {ranges.map((r) => (
            <label
              key={r.kind}
              className="flex cursor-pointer items-center gap-2 rounded border border-line px-2.5 py-1.5 text-xs has-checked:border-accent"
            >
              <input
                type="radio"
                name="send-process-range"
                checked={range === r.kind}
                onChange={() => setRange(r.kind)}
                data-testid={`send-process-range-${r.kind}`}
              />
              <span className="text-ink">
                {r.label}
                {r.count == null ? '' : t('（{n} 张）', { n: r.count })}
              </span>
            </label>
          ))}
          {range === 'filter' && filterTotal == null && (
            <p className="text-xs text-ink-dim">{t('正在数这个范围里有几张…')}</p>
          )}
          {range === 'all' && (
            <p className="text-xs text-amber-400/90">
              {t('会把库里所有图都跑一遍（已删的不算）；跑过的旧结果不会丢。')}
            </p>
          )}
        </fieldset>

        {error && (
          <p className="text-xs text-rose-300" data-testid="send-process-error" role="alert">
            {error}
          </p>
        )}
      </form>
    </Dialog>
  )
}

function CapabilityRow({
  cap,
  checked,
  onPick,
}: {
  cap: CapabilityNavItem
  checked: boolean
  onPick: () => void
}) {
  return (
    <label
      className="flex cursor-pointer items-start gap-2 rounded border border-line px-2.5 py-2 text-xs has-checked:border-accent"
      data-testid={`send-process-cap-${cap.id}`}
    >
      <input
        type="radio"
        name="send-process-cap"
        checked={checked}
        onChange={onPick}
        data-testid={`send-process-cap-input-${cap.id}`}
        className="mt-0.5"
      />
      <span className="flex min-w-0 flex-col gap-0.5">
        <span className="flex items-center gap-2">
          <span className="font-medium text-ink">{cap.label}</span>
          {/* 动词词由 manifest outputs 决定：检查=「能不能用」，处理=「变成我要的样子」 */}
          <span
            className="rounded bg-panel-2 px-1.5 py-0.5 text-[10px] text-ink-dim"
            data-testid={`send-process-cap-verb-${cap.id}`}
          >
            {capabilityVerbWord(cap.outputs)}
          </span>
        </span>
        {cap.blurb && <span className="leading-4 text-ink-dim">{cap.blurb}</span>}
      </span>
    </label>
  )
}

function PlanGroup({
  title,
  hint,
  list,
  specId,
  onPick,
}: {
  title: string
  hint: string
  list: SpecView[]
  specId: number | null
  onPick: (id: number) => void
}) {
  const { t } = useI18n()
  if (list.length === 0) return null
  return (
    <div className="flex flex-col gap-1">
      <p className="text-[11px] text-ink-dim/80">
        {title} <span className="text-ink-dim/50">{t('（{hint} · {n} 条）', { hint, n: list.length })}</span>
      </p>
      {list.map((s) => {
        const blocked = specRunBlockedReason(s)
        return (
          <label
            key={s.specId}
            className={`flex cursor-pointer items-start gap-2 rounded border px-2.5 py-1.5 text-xs has-checked:border-accent ${
              blocked ? 'border-line/60 opacity-70' : 'border-line'
            }`}
            data-testid={`send-process-plan-${s.specId}`}
          >
            <input
              type="radio"
              name="send-process-plan"
              disabled={!!blocked}
              checked={specId === s.specId}
              onChange={() => onPick(s.specId)}
              className="mt-0.5"
            />
            <span className="flex min-w-0 flex-col gap-0.5">
              <span className="font-medium text-ink">{s.name}</span>
              <span className="leading-4 text-ink-dim">{specSubtitle(s)}</span>
              {blocked && <span className="leading-4 text-amber-400/90">{blocked}</span>}
            </span>
          </label>
        )
      })}
    </div>
  )
}
