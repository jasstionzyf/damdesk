/**
 * 去重页（W5 起步 → KI-2 / KI-7 收口）—— **唯一处置面**：分组、范围、忽略、裁决、导出。
 *
 * 判据分三层呈现（不混概念，kind 分开标注，`crates/app-web/src/dedup_api.rs` 同一口径）：
 *  - `exact`       同文件（SHA-256 相同）→ 软删 + 10s 撤销窗口，不弹确认；
 *  - `near`        pHash 汉明 ≤8（压缩/重存/格式转换）→ 同上；
 *  - `near_visual` 判重塔向量余弦 ≥0.98（同一视觉内容）→ 同上；
 *  - `variant`     余弦 [0.90,0.98)（同图不同构图：裁剪/换色/加字）→ **没有「一键全处理」**，
 *                  走二次确认（文案写明「同图不同构图」），避免一次手滑删光构图变体。
 *
 * 范围（URL 单一真源，`?range=`）：`all` / `dir:<路径>` / `ids:<a,b>` / `batch:<id>`；
 * 页头只读 URL、改动只写 URL —— 刷新/分享/后退都能复现同一屏。
 *
 * 三态徽标（`judgeState`）：判重塔还没向量 = **未检测**（不是「无重复」）；补判没走完 =
 * **检测中**；走完 = **已确认**。这一条是 KI-2 的硬口径：宁可说「不知道」，也不许说「干净」。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import {
  api,
  formatSize,
  type DupGroupKind,
  type DupGroupView,
  type DupJudgeStatus,
  type DupRangeMeta,
  type AssetView,
  type DupMemberView,
} from '../lib/api'
import { can, useCapabilities } from '../lib/caps'
import { useConfirm, verbCount } from '../lib/confirm'
import { phashSimilarityPct } from '../lib/phash'
import { PathLink } from '../components/PathLink'
import { useToast } from '../lib/toast'
import { Button, buttonClass } from '../components/Button'
import { tGlobal, useI18n } from '../i18n'

const card = 'rounded-lg border border-line bg-panel p-4'

function Stat({ label, value, hint }: { label: string; value: number | string; hint?: string }) {
  return (
    <div className="rounded-md border border-line bg-panel-2 px-3 py-2">
      <div className="text-xs text-ink-dim">{label}</div>
      <div className="text-lg font-semibold">{value}</div>
      {hint && <div className="text-xs text-ink-dim">{hint}</div>}
    </div>
  )
}

export const KIND_LABEL: Record<DupGroupKind, string> = {
  exact: '精确重复（同文件）',
  near: '近似重复（pHash）',
  // KI-2b：判重塔向量层（混合组取组内最保守层，故可能与 pHash 同组出现）
  near_visual: '视觉重复（同一内容）',
  variant: '疑似变体（构图不同）',
}

/** 筛选项：近似 = pHash + 视觉两层（视觉是近似的更强判据，不属于「精确」） */
export type KindFilter = 'all' | 'exact' | 'near' | 'variant'

export const KIND_FILTERS: { key: KindFilter; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'exact', label: '精确' },
  { key: 'near', label: '近似' },
  { key: 'variant', label: '变体' },
]

/** 组是否落在当前筛选项里（near 档吃 near + near_visual） */
export function matchKind(kind: DupGroupKind, f: KindFilter): boolean {
  if (f === 'all') return true
  if (f === 'near') return kind === 'near' || kind === 'near_visual'
  return kind === f
}

/**
 * variant 组必须二次确认（KI-2：构图变体不是「重存文件」，一键全处理会误删创作版本）。
 * exact / near / near_visual 保持「软删 + 10s 撤销」的轻量路径。
 */
export function needsSecondConfirm(kind: DupGroupKind): boolean {
  return kind === 'variant'
}

/** 组内主按钮文案：动词 + 真实数量（禁止「全部」），variant 额外点明「同图不同构图」 */
export function resolveLabel(kind: DupGroupKind, removeCount: number): string {
  if (needsSecondConfirm(kind)) {
    return tGlobal('保留这张，其余 {n} 张移入回收站', { n: removeCount })
  }
  return tGlobal('保留选中，其余 {n} 张移入回收站', { n: removeCount })
}

/** 三态徽标口径（KI-2 硬口径：找不到向量时不得渲染成「无重复」） */
export type JudgeState = 'unknown' | 'unjudged' | 'running' | 'confirmed'

export function judgeState(judge?: DupJudgeStatus | null): JudgeState {
  if (!judge) return 'unknown'
  if (judge.vectors <= 0) return 'unjudged'
  if (judge.backfillDone && !judge.backfillTruncated) return 'confirmed'
  return 'running'
}

export const JUDGE_TEXT: Record<JudgeState, { label: string; hint: string; cls: string }> = {
  unknown: {
    label: '状态未知',
    hint: '拿不到判重进度：下面的组只含同文件/近似重复两类判据',
    cls: 'bg-panel-2 text-ink',
  },
  unjudged: {
    label: '未检测',
    hint: '视觉查重还没跑过：当前结果不代表「没有重复」，只代表「没算过」',
    cls: 'bg-amber-500/15 text-amber-400',
  },
  running: {
    label: '检测中',
    hint: '视觉查重还在跑：新图陆续出结果，稍后刷新',
    cls: 'bg-sky-500/15 text-sky-400',
  },
  confirmed: {
    label: '已确认',
    hint: '视觉查重已跑完整库：下面的分组是完整结论',
    cls: 'bg-emerald-500/15 text-emerald-400',
  },
}

/** 范围标签（页头只读 URL，用户要能看懂「我现在在看哪一段」） */
export function rangeLabel(range: string): string {
  if (!range || range === 'all') return tGlobal('全库')
  if (range.startsWith('dir:')) return tGlobal('文件夹 {path}', { path: range.slice(4) })
  if (range.startsWith('ids:')) {
    const n = range.slice(4).split(',').filter(Boolean).length
    return tGlobal('当前选择集（{n} 张）', { n })
  }
  if (range.startsWith('batch:')) return tGlobal('导入批次 #{id}', { id: range.slice(6) })
  return range
}

/** 范围半径之外还有重复时的提示（不许因为本范围 0 组就说「没有重复」） */
export function rangeNotice(meta?: DupRangeMeta): string | null {
  if (!meta) return null
  if (meta.kind === 'ids' && meta.outsideDupAssets > 0) {
    return tGlobal('选择集内有 {n} 张图在库里还有重复，但重复对象不在本次选择内 —— 切「全库」才能看到整组', {
      n: meta.outsideDupAssets,
    })
  }
  if ((meta.kind === 'dir' || meta.kind === 'batch') && meta.outOfScopeAssets > 0) {
    return tGlobal('本范围外还有 {n} 张图与范围内成员互为重复 —— 切「全库」才能看到完整组', {
      n: meta.outOfScopeAssets,
    })
  }
  return null
}

/** 「近似」档包含的 kind（导出/测试共用同一口径） */
export const KIND_IN_NEAR: DupGroupKind[] = ['near', 'near_visual']

/** 组内并排大图对比：大图 + 相似度 + 「保留这张」，按规则自动预选、可改选后裁决 */
function CompareModal({
  group,
  keepId,
  onKeep,
  onResolve,
  onClose,
  busy,
}: {
  group: DupGroupView
  keepId: number
  onKeep: (groupId: number, assetId: number) => void
  onResolve: (g: DupGroupView) => void
  onClose: () => void
  busy: boolean
}) {
  const { t } = useI18n()
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const members = group.members
  const keepMember = members.find((m) => m.assetId === keepId)
  const removeCount = group.size - 1

  return (
    <div
      className="fixed inset-0 z-50 flex flex-col bg-black/85"
      role="dialog"
      aria-label={t('去重组对比视图')}
      data-testid={`dedup-compare-${group.groupId}`}
    >
      <header className="flex h-12 shrink-0 items-center gap-3 border-b border-white/10 px-4 text-sm text-white">
        <span
          className={`rounded px-1.5 py-0.5 text-[10px] font-semibold ${
            group.kind === 'exact' ? 'bg-red-500/25 text-red-300' : 'bg-amber-500/25 text-amber-300'
          }`}
        >
          {t(KIND_LABEL[group.kind])}
        </span>
        <span className="text-xs text-white/70">
          {t('组 #{id} · {n} 张并排 · 点「保留这张」改选，Esc 关闭', { id: group.groupId, n: group.size })}
        </span>
        <Button aria-label={t('关闭对比')} className="ml-auto" onClick={onClose}>
          {t('✕ 关闭')}
        </Button>
      </header>

      <div className="flex min-h-0 flex-1 items-stretch gap-3 overflow-x-auto p-4">
        {members.map((m) => {
          const isKeep = m.assetId === keepId
          const sim = similarityLabel(group, m, keepMember)
          return (
            <div
              key={m.assetId}
              className={`flex min-w-[320px] flex-1 flex-col rounded-lg border p-3 ${
                isKeep ? 'border-[var(--accent)] bg-panel' : 'border-white/15 bg-panel/90'
              }`}
            >
              <img
                src={api.thumbUrl(m.assetId, 1024)}
                alt={m.fileName}
                loading="lazy"
                className={`min-h-0 w-full flex-1 rounded object-contain ${!isKeep ? 'opacity-90' : ''}`}
              />
              <div className="mt-2 space-y-1 text-xs">
                <div className="flex items-center gap-2">
                  <span className="min-w-0 flex-1 truncate font-medium" title={m.fileName}>
                    {m.fileName}
                  </span>
                  {m.assetId === group.suggestedKeep && (
                    <span
                      className="shrink-0 rounded bg-accent-soft px-1.5 py-0.5 text-[10px] text-accent"
                      title={keepReason(group)}
                    >
                      {t('自动预选')}
                    </span>
                  )}
                </div>
                {sim && (
                  <span
                    data-testid={`dedup-sim-${m.assetId}`}
                    className={`inline-block rounded px-1.5 py-0.5 text-[10px] ${
                      group.kind === 'exact'
                        ? 'bg-red-500/15 text-red-300'
                        : 'bg-amber-500/15 text-amber-300'
                    }`}
                  >
                    {sim}
                  </span>
                )}
                <div className="text-ink-dim">
                  {m.width && m.height ? `${m.width}×${m.height}` : t('尺寸未知')} · {formatSize(m.fileSz)} ·{' '}
                  {t('导入 {when}', { when: ts(m.importedAt) })}
                </div>
                {m.rejected && <div className="text-amber-400">{t('已拒选')}</div>}
                <div className="truncate text-ink-dim/70" title={m.absPath}>
                  <PathLink path={m.absPath} mode="reveal" />
                </div>
              </div>
              <Button
                active={isKeep}
                data-testid={`dedup-keep-${m.assetId}`}
                className="mt-2"
                onClick={() => onKeep(group.groupId, m.assetId)}
              >
                {isKeep ? t('✓ 保留这张') : t('保留这张')}
              </Button>
            </div>
          )
        })}
      </div>

      <footer className="flex h-14 shrink-0 items-center justify-center gap-3 border-t border-white/10">
        <Button variant="primary" size="md" disabled={busy} onClick={() => onResolve(group)}>
          {resolveLabel(group.kind, removeCount)}
        </Button>
        <span className="text-xs text-white/60">
          {keepMember ? keepReason(group) : ''}{t('（软删可恢复，不物理删除源文件）')}
        </span>
      </footer>
    </div>
  )
}

/** 自动预选理由（E5：按规则自动预选，人工只改例外） */
function keepReason(g: DupGroupView): string {
  return g.kind === 'exact' ? tGlobal('自动预选：最早导入（原件）') : tGlobal('自动预选：分辨率最高')
}

/** 与当前保留项的相似度徽标文案；null = 无法计算（缺 pHash，如 exact 组外的兜底） */
function similarityLabel(
  g: DupGroupView,
  m: DupMemberView,
  keep: DupMemberView | undefined,
): string | null {
  if (!keep || m.assetId === keep.assetId) return null
  if (g.kind === 'exact') return tGlobal('同一文件 · 100%')
  // 判重塔给过余弦就用它（向量判据比 pHash 百分数更能解释「同图不同构图」）
  if (typeof m.similarity === 'number')
    return tGlobal('相似 {pct}%', { pct: Math.round(m.similarity * 100) })
  const pct = phashSimilarityPct(m.pHash, keep.pHash)
  return pct === null ? null : tGlobal('相似 {pct}%', { pct })
}

function ts(sec: number): string {
  const d = new Date(sec * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

export function Dedup() {
  const { toast } = useToast()
  const confirm = useConfirm()
  const { t } = useI18n()
  const caps = useCapabilities()
  const [params, setParams] = useSearchParams()
  const range = params.get('range') || 'all'
  const [groups, setGroups] = useState<DupGroupView[]>([])
  const [meta, setMeta] = useState<DupRangeMeta | undefined>(undefined)
  const [judge, setJudge] = useState<DupJudgeStatus | undefined>(undefined)
  const [ignoredCount, setIgnoredCount] = useState(0)
  const [kindFilter, setKindFilter] = useState<KindFilter>('all')
  /** KI-16 #6：「只看已拒选」chip —— 与筛选芯片同形态、可取消；开=只显示含已拒选成员的组 */
  const [rejectedOnly, setRejectedOnly] = useState(false)
  // 每组当前选中的保留项（groupId → assetId）；未选 = suggestedKeep
  const [keeps, setKeeps] = useState<Record<number, number>>({})
  // E5 并排大图对比弹层
  const [compare, setCompare] = useState<DupGroupView | null>(null)
  const [trash, setTrash] = useState<AssetView[]>([])
  const [busy, setBusy] = useState(false)
  const [loaded, setLoaded] = useState(false)

  /** 范围切换 = 只改 URL（replace：改范围不该堆历史），刷新后由 URL 复现 */
  const setRange = useCallback(
    (next: string) => {
      const p = new URLSearchParams(params)
      if (!next || next === 'all') p.delete('range')
      else p.set('range', next)
      setParams(p, { replace: true })
    },
    [params, setParams],
  )

  const refresh = useCallback(async () => {
    try {
      const [resp, trashResp] = await Promise.all([api.dupGroups(range), api.dedupTrash(200)])
      setGroups(resp.groups)
      setMeta(resp.rangeMeta)
      setJudge(resp.judge)
      setIgnoredCount(resp.ignoredCount ?? 0)
      setTrash(trashResp.items)
      setLoaded(true)
    } catch {
      /* 后端瞬断：保留上次快照 */
    }
  }, [range])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const shown = useMemo(
    () =>
      groups.filter(
        (g) => matchKind(g.kind, kindFilter) && (!rejectedOnly || g.members.some((m) => m.rejected)),
      ),
    [groups, kindFilter, rejectedOnly],
  )

  /** 变体组数（页头汇总用：`variant` 永不被自动处理，必须单独计数） */
  const variantGroups = useMemo(
    () => groups.filter((g) => g.kind === 'variant').length,
    [groups],
  )

  /** 统计按**当前 groups** 现算（不是后端快照）：撤销窗口期内组是乐观移除的 */
  const stats = useMemo(
    () =>
      groups.reduce(
        (acc, g) => ({
          groups: acc.groups + 1,
          assets: acc.assets + g.size,
          bytes: acc.bytes + g.reclaimBytes,
        }),
        { groups: 0, assets: 0, bytes: 0 },
      ),
    [groups],
  )

  const keepOf = (g: DupGroupView) => keeps[g.groupId] ?? g.suggestedKeep

  const setKeep = (groupId: number, assetId: number) =>
    setKeeps((prev) => ({ ...prev, [groupId]: assetId }))

  /** 真正落库的裁决（撤销窗口过期 / 二次确认后走这里） */
  const commitResolve = async (keep: number, removeIds: number[]) => {
    setBusy(true)
    try {
      const r = await api.dedupResolve(keep, removeIds)
      window.dispatchEvent(new CustomEvent('dam:library-changed'))
      await refresh()
      toast('info', t('已裁决：保留 1 张，{n} 张移入回收站', { n: r.removed }), t('可在回收站恢复（软删）'))
    } catch (e) {
      toast('error', t('裁决失败'), String(e instanceof Error ? e.message : e))
      await refresh() // 服务端没改成 → 以服务端为准把组拉回来
    } finally {
      setBusy(false)
    }
  }

  /**
   * 裁决入口，按 kind 分两条路（KI-2）：
   * - exact / near / near_visual：软删可恢复 → 乐观移除 + 10s 撤销窗口，**不弹确认**；
   * - variant：构图变体（裁剪/换色/加字）不是「重存文件」，先二次确认再说。
   */
  const resolveGroup = async (g: DupGroupView) => {
    const keep = keepOf(g)
    const removeIds = g.members.map((m) => m.assetId).filter((id) => id !== keep)
    if (removeIds.length === 0) return
    setCompare(null)

    if (needsSecondConfirm(g.kind)) {
      const ok = await confirm({
        title: t('处理「同图不同构图」的变体组'),
        body: t('这一组是同一张图的不同构图/配色/加字版本，不一定是冗余文件。保留一张、其余移入回收站之前请先并排看过。'),
        impacts: [
          t('移入回收站：{n} 张（保留 1 张）', { n: removeIds.length }),
          t('可释放 {size}（源文件仍在磁盘）', { size: formatSize(g.reclaimBytes) }),
        ],
        recoverable: t('软删可恢复：回收站里能逐张恢复（不会物理删除源文件）'),
        confirmLabel: verbCount(t('移入回收站'), removeIds.length),
      })
      if (!ok) return
      await commitResolve(keep, removeIds)
      return
    }

    const index = groups.findIndex((x) => x.groupId === g.groupId)
    // 乐观：组当场消失（列表 + 上方统计都按 groups 现算，不出现「数字还对得上但列表少了一组」）
    setGroups((prev) => prev.filter((x) => x.groupId !== g.groupId))
    toast.undo({
      message: t('已裁决组 #{id}：保留 1 张，{n} 张移入回收站', { id: g.groupId, n: removeIds.length }),
      detail: t('10 秒内可撤销；超时才写入库（撤销后这些图仍留在图库）'),
      onUndo: () => {
        // 按原位置插回（其它组的顺序不受影响）
        setGroups((prev) =>
          prev.some((x) => x.groupId === g.groupId)
            ? prev
            : [...prev.slice(0, index), g, ...prev.slice(index)],
        )
      },
      onCommit: async () => {
        await commitResolve(keep, removeIds)
      },
    })
  }

  /** 忽略此组：立即写服务端（刷新后不复现），toast 给「撤销忽略」 */
  const ignoreGroup = async (g: DupGroupView) => {
    const index = groups.findIndex((x) => x.groupId === g.groupId)
    setGroups((prev) => prev.filter((x) => x.groupId !== g.groupId))
    setCompare(null)
    try {
      const r = await api.dedupIgnore([g.groupId])
      setIgnoredCount((n) => n + 1)
      toast.undo({
        message: t('已忽略组 #{id}（刷新后不再提示）', { id: g.groupId }),
        detail: t('组内成员有变化（又发现一张重复）时会重新出现 —— 那时再看一眼是对的'),
        onUndo: () => {
          void (async () => {
            await api.dedupUnignore(r.ignored)
            setIgnoredCount((n) => Math.max(0, n - 1))
            setGroups((prev) =>
              prev.some((x) => x.groupId === g.groupId)
                ? prev
                : [...prev.slice(0, index), g, ...prev.slice(index)],
            )
          })()
        },
        onCommit: async () => {},
      })
    } catch (e) {
      toast('error', t('忽略失败'), String(e instanceof Error ? e.message : e))
      await refresh()
    }
  }

  const undoAllIgnores = async () => {
    try {
      const r = await api.dedupUnignore([])
      toast('info', t('已恢复 {n} 组忽略项', { n: r.removed }), '')
      await refresh()
    } catch (e) {
      toast('error', t('恢复忽略失败'), String(e instanceof Error ? e.message : e))
    }
  }

  const js = judgeState(judge)
  const notice = rangeNotice(meta)
  const canScan = can(caps, 'scanExternalPath')

  return (
    <div className="mx-auto max-w-6xl space-y-4 p-6">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">{t('去重')}</h1>
          <p className="text-xs text-ink-dim">
            {t('精确（同文件）∪ 近似（压缩/重存）∪ 视觉（同内容/同图不同构图），分组后人工裁决保留项，其余移入回收站（可恢复）。')}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <a
            href={api.dedupExportUrl({ format: 'csv', range })}
            download
            data-testid="dedup-export"
            className={buttonClass()}
            title={t('导出本次范围的去重报告（CSV，含算法与阈值元数据）')}
          >
            {t('导出报告')}
          </a>
          {canScan && (
            <Button
              data-testid="dedup-scan-open"
              onClick={() => {
                const p = new URLSearchParams(params)
                p.set('dialog', 'dedup-scan')
                setParams(p, { replace: false })
              }}
            >
              {t('未入库文件夹…')}
            </Button>
          )}
        </div>
      </header>

      {/* 范围（KI-16 #6）：一个下拉，不再「按钮 + label + link」三态混放；选「全库」即清范围 */}
      <div className="flex flex-wrap items-center gap-2" data-testid="dedup-range">
        <label className="text-xs text-ink-dim" htmlFor="dedup-range-select">
          {t('范围')}
        </label>
        <select
          id="dedup-range-select"
          data-testid="dedup-range-select"
          className="rounded border border-line bg-panel-2 px-2 py-1 text-xs"
          value={range}
          onChange={(e) => setRange(e.target.value)}
        >
          <option value="all">{t('全库')}</option>
          {range !== 'all' && <option value={range}>{t('当前筛选（{label}）', { label: rangeLabel(range) })}</option>}
        </select>
      </div>

      {/* 三态徽标 + 汇总：向量没算完时明说「未检测」，不许把 0 组当「没重复」 */}
      <div className="flex flex-wrap items-center gap-3 text-xs" data-testid="dedup-status">
        <span
          className={`rounded px-2 py-0.5 font-semibold ${JUDGE_TEXT[js].cls}`}
          title={t(JUDGE_TEXT[js].hint)}
          data-testid={`judge-${js}`}
        >
          {t('视觉判重：')}{t(JUDGE_TEXT[js].label)}
        </span>
        <span className="text-ink-dim" data-testid="dedup-summary">
          {t('重复 {g} 组（可回收 {size}）· 变体 {v} 组（不会自动删）', {
            g: stats.groups,
            size: formatSize(stats.bytes),
            v: variantGroups,
          })}
        </span>
        {ignoredCount > 0 && (
          <span className="text-ink-dim" data-testid="dedup-ignored">
            {t('已忽略 {n} 组', { n: ignoredCount })}
            <Button variant="link" className="ml-2" onClick={() => void undoAllIgnores()}>
              {t('全部恢复')}
            </Button>
          </span>
        )}
      </div>

      {notice && (
        <div
          className="rounded-md border border-amber-600/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-300"
          data-testid="dedup-range-notice"
        >
          {notice}
        </div>
      )}

      <div className="grid grid-cols-3 gap-3">
        <Stat label={t('重复组')} value={stats.groups} />
        <Stat label={t('涉及资产')} value={stats.assets} />
        <Stat label={t('可释放空间')} value={formatSize(stats.bytes)} hint={t('按每组预选保留项计')} />
      </div>

      <div className="flex flex-wrap gap-2">
        {KIND_FILTERS.map((k) => (
          <Button
            key={k.key}
            active={kindFilter === k.key}
            className="rounded-full"
            onClick={() => setKindFilter(k.key)}
            data-testid={`kind-filter-${k.key}`}
          >
            {t(k.label)}
          </Button>
        ))}
        {/* KI-16 #6：「已拒选」从组内零散徽标升级为可点选/可取消的筛选 chip（与上排同形态） */}
        <Button
          active={rejectedOnly}
          className="rounded-full"
          onClick={() => setRejectedOnly((v) => !v)}
          data-testid="rejected-filter"
          title={t('只显示包含已拒选成员的重复组（再点一次取消）')}
        >
          {t('只看已拒选')}
        </Button>
      </div>

      {loaded && shown.length === 0 ? (
        <div className={`${card} text-center`} data-testid="dedup-empty">
          {js === 'unjudged' ? (
            <>
              <p className="text-sm" data-testid="dedup-unjudged-empty">
                {t('还没有视觉判重结果 —— 这不等于「没有重复」。')}
              </p>
              <p className="mt-1 text-xs text-ink-dim">
                {t('视觉查重还没跑过（当前形态可能不支持本地推理）。先在图库导入并等待后台自动处理，或在设置里配好推理后端后回到本页刷新。')}
              </p>
            </>
          ) : (
            <>
              <p className="text-sm">{t('当前范围/筛选下没有重复组。')}</p>
              <p className="mt-1 text-xs text-ink-dim">
                {t('库里没有（或还没发现）重复图。先去')}{' '}
                <Link to="/" className="text-accent hover:underline">
                  {t('图库')}
                </Link>{' '}
                {t('导入照片；近似分组依赖导入时生成的 pHash（缩略图管线自动产出）。')}
              </p>
            </>
          )}
        </div>
      ) : (
        <div className="space-y-4">
          {shown.map((g) => {
            const keep = keepOf(g)
            const removeCount = g.size - 1
            return (
              <section key={g.groupId} className={card} data-testid={`dedup-group-${g.groupId}`}>
                <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line pb-2">
                  <div className="flex items-center gap-2">
                    <span
                      className={`rounded px-1.5 py-0.5 text-[10px] font-semibold ${
                        g.kind === 'exact'
                          ? 'bg-red-500/15 text-red-400'
                          : g.kind === 'variant'
                            ? 'bg-amber-500/15 text-amber-400'
                            : 'bg-sky-500/15 text-sky-400'
                      }`}
                    >
                      {t(KIND_LABEL[g.kind])}
                    </span>
                    <span className="text-xs text-ink-dim">
                      {t('组 #{id} · {n} 张 · 可释放 {size}', {
                        id: g.groupId,
                        n: g.size,
                        size: formatSize(g.reclaimBytes),
                      })}
                      {g.mixed && t(' · 混合判据')}
                    </span>
                  </div>
                  <div className="flex items-center gap-2">
                    <Button
                      data-testid={`dedup-compare-open-${g.groupId}`}
                      onClick={() => setCompare(g)}
                    >
                      {t('并排对比')}
                    </Button>
                    <Button
                      data-testid={`dedup-ignore-${g.groupId}`}
                      title={t('这一组不用处理：以后不再提示（成员有变化会重新出现）')}
                      onClick={() => void ignoreGroup(g)}
                    >
                      {t('忽略此组')}
                    </Button>
                    <Button
                      variant="primary"
                      size="md"
                      disabled={busy}
                      data-testid={`dedup-resolve-${g.groupId}`}
                      onClick={() => void resolveGroup(g)}
                    >
                      {resolveLabel(g.kind, removeCount)}
                    </Button>
                  </div>
                </div>
                {g.kind === 'variant' && (
                  <p className="pt-2 text-[11px] text-amber-400/90" data-testid="variant-hint">
                    {t('同一张图的不同构图/配色/加字版本 —— 不会一键全处理，点上方按钮后需二次确认。')}
                  </p>
                )}
                <div className="mt-3 grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
                  {g.members.map((m) => {
                    const isKeep = m.assetId === keep
                    return (
                      <div
                        key={m.assetId}
                        className={`overflow-hidden rounded-md border ${isKeep ? 'border-accent' : 'border-line'}`}
                      >
                        <button
                          type="button"
                          className="block w-full cursor-pointer bg-panel-2"
                          title={isKeep ? t('当前保留项') : t('点选为保留项')}
                          onClick={() => setKeep(g.groupId, m.assetId)}
                        >
                          <img
                            src={api.thumbUrl(m.assetId, 256)}
                            alt={m.fileName}
                            loading="lazy"
                            className={`aspect-square w-full object-cover ${!isKeep ? 'opacity-60' : ''}`}
                          />
                        </button>
                        <div className="space-y-0.5 p-2 text-[11px] leading-4">
                          <label className="flex items-center gap-1.5">
                            <input
                              type="radio"
                              name={`keep-${g.groupId}`}
                              checked={isKeep}
                              onChange={() => setKeep(g.groupId, m.assetId)}
                            />
                            <span className="truncate font-medium" title={m.fileName}>
                              {isKeep ? t('✓ 保留：') : ''}
                              {m.fileName}
                            </span>
                          </label>
                          <div className="text-ink-dim">
                            {m.width && m.height ? `${m.width}×${m.height}` : t('尺寸未知')} ·{' '}
                            {formatSize(m.fileSz)}
                          </div>
                          <div className="text-ink-dim">{t('导入 {when}', { when: ts(m.importedAt) })}</div>
                          {m.rejected && <div className="text-amber-400">{t('已拒选')}</div>}
                          <PathLink path={m.absPath} mode="reveal" />
                        </div>
                      </div>
                    )
                  })}
                </div>
              </section>
            )
          })}
        </div>
      )}

      {/* E5 并排大图对比（裁决也在弹层内完成；裁决成功后 refresh 替换 groups） */}
      {compare && (
        <CompareModal
          group={compare}
          keepId={keepOf(compare)}
          onKeep={setKeep}
          busy={busy}
          onClose={() => setCompare(null)}
          onResolve={(g) => void resolveGroup(g)}
        />
      )}

      {/* 回收站：UI-CARRIER-1 后是独立页面 `/trash`（列表 + 分页 + 恢复）。
          本页只留计数与入口，不再内嵌可展开的长列表（同一能力两个入口 = 归属不单一）。 */}
      <section className={card}>
        <div className="flex items-center justify-between gap-3 text-sm">
          <span>
            {t('回收站（{n}）', { n: trash.length })}
            <span className="ml-2 text-xs text-ink-dim">{t('软删可恢复，完整列表在回收站页面')}</span>
          </span>
          {/* KI-16 #6：动作口径统一 —— 全页删除语义只有「移入回收站」一个词（本入口去回收站页处理/恢复） */}
          <Link
            to="/trash"
            className={buttonClass()}
            data-testid="go-trash"
            title={t('去回收站页面查看 / 恢复软删的图')}
          >
            {t('移入回收站')}
          </Link>
        </div>
      </section>
    </div>
  )
}
