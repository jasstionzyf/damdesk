/**
 * 标签总览 `/tags`（卡 task；plan 2026-10-20_113449 §2/§4）
 *
 * 定位：**全量标签可见 + 可搜可筛 + 低频清理**。不进主导航（低频但必要时要用），
 * 入口统一在图库侧栏「标签 → 全部」与命令面板「标签总览」；深链 `/tags?key=尺寸`。
 *
 * 结构与口径（都来自后端，前端不自己造数）：
 *  - 左栏 `GET /api/library/tag-groups`：key 分组（值数）。key = 值里第一个 `-` 前的段，
 *    空串组 = 无前缀裸值。**不显示「key × 资产数」**：那是每行聚合，7.5M 行实测 3.26s
 *    （见 svc-library 顶部实测表），超了「打开 <1.5s」的验收线。
 *  - 右栏 `GET /api/library/tags?page=1&key=|keyless=|prefix=`：每值计数 + 50/页 keyset 游标
 *    （`afterCount`/`afterValue`，排序 `count DESC, value ASC`）。三条路必须带范围条件，
 *    全量按计数排序后端直接 400。
 *  - 搜索框 = **全库前缀搜索**（忽略当前分组，结果行自带 key），因为跨组找值才是搜索的用途。
 *  - 点值 = 回图库 `/?tag=<完整值>`（复用图库筛选语义，不新开浏览路径）。
 *  - 清理动作（合并/改名/删除）都在本页完成：确认框数字来自 `merge-preview`（与执行同一段
 *    后端代码），执行后给回执横幅；**不提供撤销**（裁决 2026-10-20，只写 tag_merge_log 留痕）。
 */
import { useCallback, useEffect, useRef, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import {
  api,
  type TagCandidateItem,
  type TagGroupItem,
  type TagMergeOutcome,
} from '../lib/api'
import { useUrlState } from '../lib/urlState'
import { useToast } from '../lib/toast'
import { impactLine, useConfirm, verbCount } from '../lib/confirm'
import { Button, buttonClass } from '../components/Button'
import { useI18n } from '../i18n'
import { TagPicker } from '../components/TagPicker'

const PAGE = 50

/** URL 状态：`key` = 选中分组；`plain` = 无前缀裸值组（key 空串与「未选」同形，必须另外标记）；
 *  `q` = 全库前缀搜索 */
const DEFAULTS = { key: '', plain: 0, q: '' }

/** keyset 游标 = 上一页最后一行的 (count, value) */
type Cursor = { count: number; value: string } | null

const card = 'rounded-lg border border-line bg-panel'
const rowCls =
  'group flex items-center gap-2 border-b border-line/60 px-3 py-2 last:border-b-0 hover:bg-panel-2'

export function TagBrowser() {
  const ui = useUrlState(DEFAULTS)
  const navigate = useNavigate()
  const { toast } = useToast()
  const confirm = useConfirm()
  const { t } = useI18n()

  const [groups, setGroups] = useState<TagGroupItem[]>([])
  const [groupsTotal, setGroupsTotal] = useState(0)
  const [groupsBusy, setGroupsBusy] = useState(true)

  const [rows, setRows] = useState<TagCandidateItem[]>([])
  const [rowTotal, setRowTotal] = useState(0)
  const [hasMore, setHasMore] = useState(false)
  const [rowsBusy, setRowsBusy] = useState(false)
  /** 游标栈：索引 = 页码，`[0] = null` 是第一页；上一页 = 弹栈（keyset 不能反算） */
  const [cursors, setCursors] = useState<Cursor[]>([null])
  const page = cursors.length - 1

  const [samples, setSamples] = useState<Record<string, number[]>>({})
  const [sel, setSel] = useState<string[]>([])
  const [receipt, setReceipt] = useState<string | null>(null)
  /** 合并/改名对话框：values = 源，into = 目标，rename = 单值改名模式 */
  const [dialog, setDialog] = useState<{ values: string[]; into: string; rename: boolean } | null>(
    null,
  )
  const [busy, setBusy] = useState(false)

  // 回执 5s 后淡出（plan §4-C 第 4 条）
  useEffect(() => {
    if (!receipt) return
    const t = window.setTimeout(() => setReceipt(null), 5000)
    return () => window.clearTimeout(t)
  }, [receipt])

  const loadGroups = useCallback(async () => {
    setGroupsBusy(true)
    try {
      const r = await api.tagGroups()
      setGroups(r.items)
      setGroupsTotal(r.total)
    } catch (e) {
      toast('error', t('标签分组加载失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setGroupsBusy(false)
    }
  }, [toast, t])

  useEffect(() => {
    void loadGroups()
  }, [loadGroups])

  const cursor = cursors[page] ?? null
  const key = ui.value.key
  const prefix = ui.value.q.trim()
  /** 搜索态：全库前缀搜索（忽略分组） */
  const searching = prefix.length > 0
  const plain = ui.value.plain === 1
  const keyless = !searching && plain

  /**
   * 首次进入（URL 没带 key/plain/搜索）→ 默认选中值数最多的组。
   * 理由：全量值列表按计数排序要 3.26s（后端 400 拦掉），所以「没有范围」不是一个可展示的状态；
   * 默认落到第一组，用户点左侧任何一组即可切换（不整页刷新）。
   */
  useEffect(() => {
    if (searching || plain || key !== '' || groups.length === 0) return
    ui.set({ key: groups[0].key })
    // ui.set 每次渲染都是新函数，依赖它会导致抖动；只跟这五个状态走
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searching, plain, key, groups])

  const loadRows = useCallback(async () => {
    // 未定范围（首次进入、groups 还没到）→ 不发请求：后端会 400（全量按计数排序代价过高）
    if (!searching && !plain && key === '') return
    setRowsBusy(true)
    try {
      const r = await api.tagPage({
        key: searching || keyless ? undefined : key,
        keyless: keyless ? true : undefined,
        prefix: searching ? prefix : undefined,
        limit: PAGE,
        after: cursor,
      })
      setRows(r.items)
      setRowTotal(r.total)
      setHasMore(r.hasMore)
      // 样本图：值 → 最多 3 个 assetId（失败不阻塞列表，缩略图位置显示占位）
      const values = r.items.map((x) => x.value)
      if (values.length > 0) {
        try {
          const s = await api.tagSamples(values, 3)
          setSamples((prev) => {
            const next = { ...prev }
            for (const it of s.items) next[it.value] = it.assetIds
            return next
          })
        } catch {
          /* 样本图非关键路径 */
        }
      }
    } catch (e) {
      toast('error', t('标签值加载失败'), e instanceof Error ? e.message : String(e))
      setRows([])
      setRowTotal(0)
      setHasMore(false)
    } finally {
      setRowsBusy(false)
    }
  }, [searching, keyless, key, prefix, cursor, toast, t])

  useEffect(() => {
    void loadRows()
  }, [loadRows])

  // 分组/搜索变化 = 换结果集：页码回到第 1 页，选择集清空
  const lastScope = useRef('')
  useEffect(() => {
    const scope = `${plain ? 'plain' : key}\u0000${prefix}`
    if (lastScope.current === scope) return
    lastScope.current = scope
    setCursors([null])
    setSel([])
  }, [plain, key, prefix])

  /** 分组头：搜索态/裸值组各有自己的说法 */
  const scopeLabel = searching
    ? t('搜索「{q}」（全库）', { q: prefix })
    : plain || key === ''
      ? t('（无前缀）')
      : key

  const toggle = (value: string) =>
    setSel((prev) => (prev.includes(value) ? prev.filter((v) => v !== value) : [...prev, value]))

  const rowCount = (value: string) => rows.find((r) => r.value === value)?.count ?? 0

  const openMerge = (values: string[], rename = false) => {
    if (values.length === 0) return
    setDialog({ values, into: rename ? values[0] : '', rename })
  }

  /** 合并/改名：预览 → 确认 → 执行 → 回执（数字全部来自后端预览，前端不自己算） */
  const runMerge = async () => {
    if (!dialog) return
    const into = dialog.into.trim()
    if (!into) {
      toast('error', t('请先选或输入目标标签'))
      return
    }
    setBusy(true)
    try {
      const prev: TagMergeOutcome = await api.tagMergePreview(dialog.values, into)
      const ok = await confirm({
        title: dialog.rename ? t('改名') : t('合并标签'),
        body: t('把 {n} 个标签值合并到「{kept}」。', { n: dialog.values.length, kept: prev.keptValue }),
        impacts: [
          impactLine(t('受影响资产'), prev.affected, t('张')) ?? '',
          prev.removedValues.length > 0 ? t('将消失的值：{vals}', { vals: prev.removedValues.join('、') }) : '',
        ].filter(Boolean),
        recoverable: t('不可撤销（只写合并留痕，不做回收站）'),
        confirmLabel: verbCount(t('合并'), prev.affected, t('张')),
      })
      if (!ok) return
      const out = await api.tagMerge(dialog.values, into)
      setReceipt(
        t('已把 {vals} 合并到「{kept}」，影响 {n} 张', {
          vals: out.removedValues.map((v) => `「${v}」`).join('、'),
          kept: out.keptValue,
          n: out.affected,
        }),
      )
      setDialog(null)
      setSel([])
      setCursors([null])
      await loadGroups()
      await loadRows()
    } catch (e) {
      toast('error', t('合并失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  /** 删除：只在库计数为 0 才允许（后端硬拒，这里先给可读原因，避免整批往返） */
  const runDelete = async (values: string[]) => {
    if (values.length === 0) return
    const withAssets = values.filter((v) => rowCount(v) > 0)
    if (withAssets.length > 0) {
      toast(
        'error',
        t('这些标签还有资产，不能删除'),
        t('{vals} —— 请先合并到其它值', {
          vals: withAssets.map((v) => `「${v}」${rowCount(v)} ${t('张')}`).join('、'),
        }),
      )
      return
    }
    const ok = await confirm({
      title: t('删除标签值'),
      body: t('删除 {n} 个没有资产的标签值。', { n: values.length }),
      impacts: [t('值：{vals}', { vals: values.join('、') })],
      recoverable: t('不可撤销（只写删除留痕）'),
      confirmLabel: verbCount(t('删除'), values.length, t('个')),
      danger: true,
    })
    if (!ok) return
    setBusy(true)
    try {
      const out = await api.tagDelete(values)
      setReceipt(t('已删除 {n} 个标签值（清理 {m} 行）', { n: out.deleted.length, m: out.removedRows }))
      setSel([])
      await loadGroups()
      await loadRows()
    } catch (e) {
      toast('error', t('删除被拒绝'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col gap-3 p-4" data-testid="tag-browser">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-lg font-semibold">{t('标签总览')}</h1>
        <span className="text-xs text-ink-dim" data-testid="tag-browser-summary">
          {t('共 {n} 个标签值 · {m} 组', { n: groupsTotal, m: groups.length })}
        </span>
        <div className="ml-auto flex items-center gap-2">
          <input
            data-testid="tag-search"
            value={ui.value.q}
            onChange={(e) => ui.set({ q: e.target.value })}
            placeholder={t('搜索标签（前缀，全库）…')}
            className="w-64 rounded border border-line bg-panel-2 px-3 py-1.5 text-sm outline-none focus:border-accent"
          />
          <Link to="/" className={buttonClass({ variant: 'secondary', size: 'sm' })}>
            {t('回图库')}
          </Link>
        </div>
      </div>

      {receipt && (
        <div
          data-testid="tag-receipt"
          className="rounded border border-accent/50 bg-accent-soft px-3 py-2 text-sm"
        >
          {receipt}
        </div>
      )}

      <div className="flex min-h-0 flex-1 gap-3">
        {/* 左栏：key 分组（值数） */}
        <aside className={`${card} flex w-56 shrink-0 flex-col overflow-y-auto`}>
          <div className="border-b border-line px-3 py-2 text-xs font-semibold tracking-wider text-ink-dim">
            {t('分组（按值数）')}
          </div>
          {groupsBusy && <p className="px-3 py-2 text-xs text-ink-dim">{t('统计中…')}</p>}
          {groups.map((g) => {
            const active = searching
              ? false
              : g.key === ''
                ? plain
                : !plain && key === g.key
            return (
              <button
                key={g.key || 'plain'}
                type="button"
                data-testid={`tag-group-${g.key || 'plain'}`}
                onClick={() => ui.set(g.key === '' ? { key: '', plain: 1 } : { key: g.key, plain: 0 })}
                className={`flex items-center gap-2 px-3 py-2 text-left text-sm hover:bg-panel-2 ${
                  active ? 'bg-accent-soft font-medium' : ''
                }`}
              >
                <span className="min-w-0 flex-1 truncate">{g.key || t('（无前缀）')}</span>
                <span className="shrink-0 text-xs text-ink-dim">{t('{n} 值', { n: g.values })}</span>
              </button>
            )
          })}
          {!groupsBusy && groups.length === 0 && (
            <p className="px-3 py-2 text-xs leading-5 text-ink-dim">{t('暂无标签（打标后出现）')}</p>
          )}
        </aside>

        {/* 右栏：值列表 + 批量条 */}
        <section className={`${card} flex min-w-0 flex-1 flex-col`}>
          <div className="flex flex-wrap items-center gap-2 border-b border-line px-3 py-2">
            <span className="text-sm font-medium" data-testid="tag-scope">
              {scopeLabel || t('（无前缀）')}
            </span>
            <span className="text-xs text-ink-dim" data-testid="tag-scope-total">
              {t('{n} 个值', { n: rowTotal })}
            </span>
            {sel.length > 0 && (
              <span className="ml-2 flex items-center gap-2" data-testid="tag-batch-bar">
                <span className="text-xs text-ink-dim">{t('已选 {n} 项', { n: sel.length })}</span>
                <Button
                  size="sm"
                  variant="secondary"
                  data-testid="tag-batch-merge"
                  onClick={() => openMerge(sel)}
                  disabled={busy}
                >
                  {t('合并到…')}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  tone="danger"
                  data-testid="tag-batch-delete"
                  onClick={() => void runDelete(sel)}
                  disabled={busy}
                >
                  {t('删除')}
                </Button>
                <Button size="sm" variant="secondary" onClick={() => setSel([])}>
                  {t('清空')}
                </Button>
              </span>
            )}
            <span className="ml-auto flex items-center gap-2">
              <Button
                size="sm"
                variant="secondary"
                data-testid="tag-prev"
                disabled={page === 0}
                onClick={() => setCursors((c) => c.slice(0, -1))}
              >
                {t('上一页')}
              </Button>
              <span className="text-xs text-ink-dim">
                {t('第 {a} / {b} 页', { a: page + 1, b: Math.max(1, Math.ceil(rowTotal / PAGE)) })}
              </span>
              <Button
                size="sm"
                variant="secondary"
                data-testid="tag-next"
                disabled={!hasMore || rows.length === 0}
                onClick={() => {
                  const last = rows[rows.length - 1]
                  setCursors((c) => [...c, { count: last.count, value: last.value }])
                }}
              >
                {t('下一页')}
              </Button>
            </span>
          </div>

          {rowsBusy && <p className="px-3 py-3 text-xs text-ink-dim">{t('加载中…')}</p>}
          {!rowsBusy && rows.length === 0 && (
            <p className="px-3 py-3 text-xs text-ink-dim" data-testid="tag-empty">
              {searching ? t('没有匹配「{q}」的标签', { q: prefix }) : t('这一组暂时没有标签值')}
            </p>
          )}

          <div className="min-h-0 flex-1 overflow-y-auto">
            {rows.map((r) => (
              <div key={r.value} className={rowCls} data-testid={`tag-row-${r.value}`}>
                <input
                  type="checkbox"
                  aria-label={t('选择 {v}', { v: r.value })}
                  data-testid={`tag-check-${r.value}`}
                  checked={sel.includes(r.value)}
                  onChange={() => toggle(r.value)}
                  className="h-4 w-4 shrink-0 accent-accent"
                />
                <button
                  type="button"
                  onClick={() => navigate(`/?tag=${encodeURIComponent(r.value)}`)}
                  title={t('在图库中筛选「{v}」', { v: r.value })}
                  className="min-w-0 flex-1 truncate text-left text-sm hover:text-accent"
                >
                  {r.value}
                </button>
                <span className="shrink-0 text-xs text-ink-dim">{t('{n} 张', { n: r.count })}</span>
                <span className="flex shrink-0 items-center gap-1">
                  {(samples[r.value] ?? []).slice(0, 3).map((id) => (
                    <img
                      key={id}
                      src={api.thumbUrl(id, 256)}
                      alt=""
                      loading="lazy"
                      data-testid={`tag-sample-${r.value}-${id}`}
                      className="h-8 w-8 rounded border border-line object-cover"
                    />
                  ))}
                </span>
                <span className="flex shrink-0 items-center gap-1 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
                  <Button size="sm" variant="link" onClick={() => openMerge([r.value])}>
                    {t('合并到…')}
                  </Button>
                  <Button size="sm" variant="link" onClick={() => openMerge([r.value], true)}>
                    {t('改名')}
                  </Button>
                  <Button size="sm" variant="link" tone="danger" onClick={() => void runDelete([r.value])}>
                    {t('删除')}
                  </Button>
                </span>
              </div>
            ))}
          </div>
        </section>
      </div>

      {dialog && (
        <MergeDialog
          dialog={dialog}
          busy={busy}
          onChange={(into) => setDialog((d) => (d ? { ...d, into } : d))}
          onCancel={() => setDialog(null)}
          onConfirm={() => void runMerge()}
        />
      )}
    </div>
  )
}

/** 合并/改名对话框：目标值用统一 TagPicker（tag 场景 = 允许输入新值）。 */
function MergeDialog({
  dialog,
  busy,
  onChange,
  onCancel,
  onConfirm,
}: {
  dialog: { values: string[]; into: string; rename: boolean }
  busy: boolean
  onChange: (into: string) => void
  onCancel: () => void
  onConfirm: () => void
}) {
  const { t } = useI18n()
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      role="dialog"
      aria-label={dialog.rename ? t('改名') : t('合并标签')}
      data-testid="tag-merge-dialog"
    >
      <div className="w-full max-w-md rounded-lg border border-line bg-panel p-4">
        <h2 className="text-base font-semibold">{dialog.rename ? t('改名') : t('合并到…')}</h2>
        <p className="mt-1 text-xs leading-5 text-ink-dim">
          {t('源（{n}）：', { n: dialog.values.length })}
          {dialog.values.map((v) => `「${v}」`).join('、')}
        </p>
        <div className="mt-3">
          <label className="text-xs text-ink-dim" htmlFor="tag-merge-target">
            {t('目标标签（可输入新值）')}
          </label>
          <div className="mt-1">
            <TagPicker
              testid="tag-merge-target"
              value={dialog.into}
              onChange={onChange}
              onPick={onChange}
              onCommit={onChange}
              scenario="tag"
              placeholder={t('选已有标签或输入新值…')}
              className="w-full rounded border border-line bg-panel-2 px-3 py-1.5 text-sm outline-none focus:border-accent"
            />
          </div>
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="secondary" onClick={onCancel} disabled={busy}>
            {t('取消')}
          </Button>
          <Button variant="primary" data-testid="tag-merge-confirm" onClick={onConfirm} disabled={busy}>
            {t('下一步（确认影响）')}
          </Button>
        </div>
      </div>
    </div>
  )
}
